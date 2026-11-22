pub const MODEL_ID: &str = "intfloat/multilingual-e5-small";
pub const MODEL_REVISION: &str = "614241f622f53c4eeff9890bdc4f31cfecc418b3";
pub const DIMENSIONS: usize = 384;
pub const MODEL_FILES: [(&str, &str); 5] = [
    (
        "onnx/model.onnx",
        "ca456c06b3a9505ddfd9131408916dd79290368331e7d76bb621f1cba6bc8665",
    ),
    (
        "tokenizer.json",
        "0b44a9d7b51c3c62626640cda0e2c2f70fdacdc25bbbd68038369d14ebdf4c39",
    ),
    (
        "config.json",
        "69137736cab8b8903a07fe8afaafdda25aac55415a12a55d1bffa9f581abf959",
    ),
    (
        "special_tokens_map.json",
        "d05497f1da52c5e09554c0cd874037a083e1dc1b9cfd48034d1c717f1afc07a7",
    ),
    (
        "tokenizer_config.json",
        "a1d6bc8734a6f635dc158508bef000f8e2e5a759c7d92f984b2c86e5ff53425b",
    ),
];

pub fn runtime_filename() -> &'static str {
    if cfg!(target_os = "macos") {
        "libonnxruntime.dylib"
    } else if cfg!(windows) {
        "onnxruntime.dll"
    } else {
        "libonnxruntime.so"
    }
}

#[cfg(feature = "local-embeddings")]
mod local;
#[cfg(feature = "local-embeddings")]
pub use local::Embeddings;

#[cfg(not(feature = "local-embeddings"))]
pub struct Embeddings {
    _private: (),
}

#[cfg(not(feature = "local-embeddings"))]
impl Embeddings {
    pub fn install(_: &std::path::Path, _: &std::path::Path) -> crate::Result<()> {
        Err("Local embeddings are disabled in this build".into())
    }

    pub fn load(_: &std::path::Path) -> crate::Result<Self> {
        Err("Local embeddings are disabled in this build".into())
    }

    pub fn query(&mut self, _: &str) -> crate::Result<Vec<f32>> {
        Err("Local embeddings are disabled in this build".into())
    }

    pub fn passages(&mut self, _: &[String]) -> crate::Result<Vec<Vec<f32>>> {
        Err("Local embeddings are disabled in this build".into())
    }
}
