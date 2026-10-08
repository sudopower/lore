use anyhow::Result;

use super::{DIM, Embedder};

/// Deterministic bag-of-tokens embedder for tests and `make run`. Texts that share tokens get a
/// higher cosine similarity, which is enough to exercise retrieval without a model.
#[derive(Debug, Clone, Copy, Default)]
pub struct HashEmbedder;

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h = FNV_OFFSET;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

fn embed(text: &str) -> Vec<f32> {
    let lower = text.to_lowercase();
    let mut v = vec![0.0f32; DIM];
    for token in lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.chars().count() >= 2)
    {
        let h = fnv1a(token.as_bytes());
        let idx = (h % DIM as u64) as usize;
        let sign = if (h / DIM as u64) & 1 == 0 { 1.0 } else { -1.0 };
        v[idx] += sign;
    }
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm == 0.0 {
        // No tokens (or they cancelled out): a fixed unit vector.
        v[0] = 1.0;
    } else {
        for x in &mut v {
            *x /= norm;
        }
    }
    v
}

impl Embedder for HashEmbedder {
    fn name(&self) -> &str {
        "hash"
    }

    fn embed_documents(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|t| embed(t)).collect())
    }

    fn embed_query(&self, text: &str) -> Result<Vec<f32>> {
        Ok(embed(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cosine(a: &[f32], b: &[f32]) -> f32 {
        a.iter().zip(b).map(|(x, y)| x * y).sum()
    }

    #[test]
    fn same_input_gives_same_output() {
        let e = HashEmbedder;
        assert_eq!(
            e.embed_query("plan limits seats").unwrap(),
            e.embed_query("plan limits seats").unwrap()
        );
        let docs = e.embed_documents(&["a b cc".into()]).unwrap();
        assert_eq!(docs, e.embed_documents(&["a b cc".into()]).unwrap());
    }

    #[test]
    fn dimension_is_384_and_vectors_are_unit_length() {
        let e = HashEmbedder;
        let v = e.embed_query("plan limits seats").unwrap();
        assert_eq!(v.len(), 384);
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5, "norm {norm}");
    }

    #[test]
    fn shared_tokens_rank_higher() {
        let e = HashEmbedder;
        let q = e.embed_query("plan limits seats").unwrap();
        let near = e.embed_query("limits for each plan").unwrap();
        let far = e.embed_query("theme toggle button").unwrap();
        assert!(cosine(&q, &near) > cosine(&q, &far));
    }

    #[test]
    fn empty_text_is_the_first_basis_vector() {
        let e = HashEmbedder;
        for text in ["", "   ", "a !"] {
            let v = e.embed_query(text).unwrap();
            assert_eq!(v.len(), DIM);
            assert_eq!(v[0], 1.0);
            assert!(v[1..].iter().all(|x| *x == 0.0), "{text:?}");
        }
    }

    #[test]
    fn case_and_punctuation_are_ignored() {
        let e = HashEmbedder;
        assert_eq!(
            e.embed_query("Plan, LIMITS!").unwrap(),
            e.embed_query("plan limits").unwrap()
        );
    }

    #[test]
    fn documents_come_back_in_order() {
        let e = HashEmbedder;
        let out = e
            .embed_documents(&["alpha beta".into(), "gamma delta".into()])
            .unwrap();
        assert_eq!(out[0], e.embed_query("alpha beta").unwrap());
        assert_eq!(out[1], e.embed_query("gamma delta").unwrap());
        assert_eq!(e.name(), "hash");
    }
}
