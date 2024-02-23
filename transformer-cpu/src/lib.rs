mod cache;
mod kernel;
mod storage;

use cache::LayerCache;
use common::{upos, utok};
use kernel::{gather, matmul, rms_norm, rotary_embedding, softmax};
use model_parameters::{Llama2, Memory};
use storage::Storage;
use tensor::{reslice, slice, udim, DataType, Tensor};

pub extern crate model_parameters;

pub struct Transformer {
    model: Box<dyn Llama2>,
}

impl Transformer {
    #[inline]
    pub fn new(model: Box<dyn Llama2>) -> Self {
        Self {
            model: match model.data_type() {
                DataType::BF16 => Box::new(Memory::cast(&*model, DataType::F32)),
                _ => model,
            },
        }
    }

    #[inline]
    pub fn new_cache(&self) -> Vec<LayerCache> {
        LayerCache::new_layers(&*self.model)
    }

    pub fn update(&self, tokens: &[utok], cache: &mut [LayerCache], pos: upos) -> Vec<f32> {
        let seq_len = tokens.len() as udim;
        let d = self.model.hidden_size() as udim;
        let nh = self.model.num_attention_heads() as udim;
        let nkvh = self.model.num_key_value_heads() as udim;
        let dh = d / nh;
        let dkv = nkvh * dh;
        let head_group = nh / nkvh;
        let head_div = (dh as f32).sqrt().recip();
        let di = self.model.intermediate_size() as udim;
        let dt = self.model.data_type();
        let epsilon = self.model.rms_norm_eps();
        let theta = self.model.rope_theta();
        let att_len = pos + seq_len;
        let cat_slice = &[slice![all], slice![pos; 1; seq_len], slice![all]];
        let att_slice = &[slice![all], slice![  0; 1; att_len], slice![all]];
        let pos = (pos..pos + seq_len).collect::<Vec<udim>>();
        let pos = Tensor::new(DataType::U32, &[seq_len], reslice::<udim, u8>(&pos));
        // println!("tokens: {tokens:?}");

        let mut x0 = tensor(dt, &[seq_len, d]);
        let mut x1 = tensor(dt, &[seq_len, d]);
        let mut x2 = tensor(dt, &[nkvh, head_group * seq_len, dh]);
        let mut qkv = tensor(dt, &[seq_len, d + dkv + dkv]);
        let mut q_att = tensor(dt, &[nh, seq_len, dh]);
        let mut att = tensor(dt, &[nkvh, head_group * seq_len, att_len]);
        let mut gate_up = tensor(dt, &[seq_len, di]);

        gather(&mut x0.access_mut(), &self.model.embed_tokens(), tokens);
        // println!("gather:\n{}", x0.access());

        for layer in 0..self.model.num_hidden_layers() {
            let input_layernorm = self.model.input_layernorm(layer);
            rms_norm(
                &mut x1.access_mut(),
                &x0.access(),
                &input_layernorm,
                epsilon,
            );
            // println!("layer {layer} input norm:\n{}", x1.access());
            let w_qkv = self.model.w_qkv(layer).transpose(&[1, 0]);
            matmul(&mut qkv.access_mut(), 0., &x1.access_mut(), &w_qkv, 1.);
            let mut qkv = qkv.split(1, &[d as _, dkv as _, dkv as _]);
            let v = qkv.pop().unwrap().reshape(&[seq_len, nkvh, dh]);
            let mut k = qkv.pop().unwrap().reshape(&[seq_len, nkvh, dh]);
            let mut q = qkv.pop().unwrap().reshape(&[seq_len, nh, dh]);
            // println!("layer {layer} q:\n{}", q.access());
            // println!("layer {layer} k:\n{}", k.access());
            // println!("layer {layer} v:\n{}", v.access());
            rotary_embedding(&mut q.access_mut(), &pos, theta);
            rotary_embedding(&mut k.access_mut(), &pos, theta);
            // println!("layer {layer} rot q:\n{}", q.access());
            // println!("layer {layer} rot k:\n{}", k.access());
            let q = q.transpose(&[1, 0, 2]);
            let k = k.transpose(&[1, 0, 2]);
            let v = v.transpose(&[1, 0, 2]);

            let (k_cache, v_cache) = cache[layer].get();
            let mut k_cat = k_cache.slice(cat_slice);
            let mut v_cat = v_cache.slice(cat_slice);
            q.access().reform_to(&mut q_att.access_mut());
            k.access().reform_to(&mut k_cat.access_mut());
            v.access().reform_to(&mut v_cat.access_mut());

            let q_att = q_att.clone().reshape(&[nkvh, head_group * seq_len, dh]);
            let k_att = k_cache.slice(att_slice);
            let v_att = v_cache.slice(att_slice);
            // println!("layer {layer} q attention:\n{}", q_att.access());
            // println!("layer {layer} k attention:\n{}", k_att.access());
            // println!("layer {layer} v attention:\n{}", v_att.access());

            {
                let k_att = k_att.transpose(&[0, 2, 1]);
                matmul(
                    &mut att.access_mut(),
                    0.,
                    &q_att.access(),
                    &k_att.access(),
                    head_div,
                );
                {
                    let mut att = att.clone().reshape(&[nh, seq_len, att_len]);
                    softmax(&mut att.access_mut());
                }
                matmul(&mut x2.access_mut(), 0., &att.access(), &v_att.access(), 1.);
            }
            {
                let x2 = x2.clone().reshape(&[nh, seq_len, dh]).transpose(&[1, 0, 2]);
                let mut x1 = x1.clone().reshape(&[seq_len, nh, dh]);
                x2.access().reform_to(&mut x1.access_mut());
            }
            // println!("layer {layer} after attention:\n{}", x1.access());
            let wo = self.model.self_attn_o_proj(layer).transpose(&[1, 0]);
            matmul(&mut x0.access_mut(), 1., &x1.access(), &wo, 1.);
            // println!("layer {layer} o_proj:\n{}", x0.access());
            let post_layernorm = self.model.post_attention_layernorm(layer);
            rms_norm(&mut x1.access_mut(), &x0.access(), &post_layernorm, epsilon);
            // println!("layer {layer} post norm:\n{}", x1.access());
            let w_gate_up = self.model.mlp_gate_up(layer).transpose(&[1, 0]);
            matmul(&mut gate_up.access_mut(), 0., &x1.access(), &w_gate_up, 1.);
            let mut gate_up = gate_up.split(1, &[di as _, di as _]);
            let _up = gate_up.pop().unwrap();
            let _gate = gate_up.pop().unwrap();
            // println!("layer {layer} gate:\n{}", gate.access());
            // println!("layer {layer} up:\n{}", up.access());
        }

        vec![]
    }
}

#[inline]
fn tensor(dt: DataType, shape: &[udim]) -> Tensor<Storage> {
    Tensor::new(
        dt,
        shape,
        Storage::new(shape.iter().product::<udim>() as usize * dt.size()),
    )
}

#[test]
fn test_build() {
    use model_parameters::SafeTensorError;
    use std::time::Instant;

    let t0 = Instant::now();
    let safetensors = Memory::load_safetensors("../../TinyLlama-1.1B-Chat-v1.0");
    let t1 = Instant::now();
    println!("mmap {:?}", t1 - t0);

    let safetensors = match safetensors {
        Ok(m) => m,
        Err(SafeTensorError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => panic!("{e:?}"),
    };

    let t0 = Instant::now();
    let _transformer = Transformer::new(Box::new(safetensors));
    let t1 = Instant::now();
    println!("build transformer {:?}", t1 - t0);
}
