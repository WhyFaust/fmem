use super::{runtime_filename, DIMENSIONS, MODEL_FILES, MODEL_ID, MODEL_REVISION};
use crate::{memory::io, Result};
use fastembed::{
    InitOptionsUserDefined, Pooling, TextEmbedding, TokenizerFiles, UserDefinedEmbeddingModel,
};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Mutex,
};

const MODEL_FILE_SIZES: [u64; 5] = [470_268_510, 17_082_730, 655, 167, 443];
const MAX_TOKENS: usize = 512;
const BATCH_SIZE: usize = 8;

static RUNTIME: Mutex<Option<libloading::Library>> = Mutex::new(None);

pub struct Embeddings {
    model: TextEmbedding,
}

impl Embeddings {
    pub fn install(state: &Path, source: &Path) -> Result<()> {
        let state = io::absolute(state)?;
        io::profile_state(&state, true)?;
        let target = model_directory(&state);
        let parent = target.parent().ok_or("Missing model parent directory")?;
        io::directory(parent, true, true)?;
        let _lock = io::lock_key(
            &format!("fmem-model-{}", io::hash(io::directory_key(parent))),
            None,
        )?;
        if io::directory(&target, false, false)? && model_files_verified(&target)? {
            return Ok(());
        }
        let staging = tempfile::Builder::new()
            .prefix(".install-")
            .tempdir_in(parent)
            .map_err(|error| format!("Could not stage the embedding model: {error}"))?;
        for ((relative, expected), size) in MODEL_FILES.into_iter().zip(MODEL_FILE_SIZES) {
            let input = source.join(relative);
            check_model_file(&input, size)?;
            let output = staging.path().join(relative);
            io::directory(
                output.parent().ok_or("Missing model file parent")?,
                true,
                true,
            )?;
            let mut output = io::options()
                .create_new(true)
                .write(true)
                .open(&output)
                .map_err(io::err)?;
            let actual = hash_model_file(&input, size, &mut output)?;
            if actual != expected {
                return Err(format!(
                    "Embedding model checksum does not match {relative}"
                ));
            }
            output.sync_all().map_err(io::err)?;
        }
        io::sync_parent(&staging.path().join(MODEL_FILES[0].0))?;
        io::sync_parent(&staging.path().join(MODEL_FILES[1].0))?;
        if io::directory(&target, false, false)? {
            let previous = tempfile::Builder::new()
                .prefix(".previous-")
                .tempdir_in(parent)
                .map_err(io::err)?;
            let backup = previous.path().join("model");
            fs::rename(&target, &backup).map_err(io::err)?;
            if let Err(error) = fs::rename(staging.path(), &target) {
                fs::rename(&backup, &target).map_err(io::err)?;
                return Err(format!("Could not replace the embedding model: {error}"));
            }
        } else {
            fs::rename(staging.path(), &target).map_err(io::err)?;
        }
        io::sync_parent(&target)?;
        Ok(())
    }

    pub fn installed(state: &Path) -> Result<bool> {
        let directory = model_directory(state);
        if !io::directory(&directory, false, false)?
            || !io::file(&state.join("models").join(runtime_filename()))?
        {
            return Ok(false);
        }
        for ((relative, _), size) in MODEL_FILES.into_iter().zip(MODEL_FILE_SIZES) {
            let path = directory.join(relative);
            if !io::file(&path)? || fs::metadata(path).map_err(io::err)?.len() != size {
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub fn load_installed(state: &Path) -> Result<Self> {
        Self::load_model(&io::absolute(state)?, true)
    }

    pub fn load(state: &Path) -> Result<Self> {
        Self::load_model(state, false)
    }

    fn load_model(state: &Path, installed_only: bool) -> Result<Self> {
        let directory = model_directory(state);
        if !io::directory(&directory, false, false)? {
            return Err(format!("Embedding model {MODEL_ID} is not installed"));
        }
        for ((relative, _), size) in MODEL_FILES.into_iter().zip(MODEL_FILE_SIZES) {
            check_model_file(&directory.join(relative), size)?;
        }
        initialize_runtime(state, installed_only)?;
        let bytes = |relative: &str| {
            fs::read(directory.join(relative))
                .map_err(|error| format!("Could not read embedding model file {relative}: {error}"))
        };
        let model = TextEmbedding::try_new_from_user_defined(
            UserDefinedEmbeddingModel::new(
                bytes("onnx/model.onnx")?,
                TokenizerFiles {
                    tokenizer_file: bytes("tokenizer.json")?,
                    config_file: bytes("config.json")?,
                    special_tokens_map_file: bytes("special_tokens_map.json")?,
                    tokenizer_config_file: bytes("tokenizer_config.json")?,
                },
            )
            .with_pooling(Pooling::Mean),
            InitOptionsUserDefined::new().with_max_length(MAX_TOKENS),
        )
        .map_err(|error| format!("Could not load {MODEL_ID}: {error}"))?;
        Ok(Self { model })
    }

    pub fn query(&mut self, text: &str) -> Result<Vec<f32>> {
        self.embed(&[format!("query: {text}")])?
            .pop()
            .ok_or_else(|| "Embedding model did not return a query vector".into())
    }

    pub fn passages(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        self.embed(
            &texts
                .iter()
                .map(|text| format!("passage: {text}"))
                .collect::<Vec<_>>(),
        )
    }

    fn embed(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let mut vectors = self
            .model
            .embed(texts, Some(BATCH_SIZE))
            .map_err(|error| format!("Embedding inference failed: {error}"))?;
        if vectors.len() != texts.len() {
            return Err("Embedding model returned an unexpected number of vectors".into());
        }
        for vector in &mut vectors {
            let length = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
            if vector.len() != DIMENSIONS || !length.is_finite() || length <= 0.0 {
                return Err("Embedding model returned an invalid vector".into());
            }
            vector.iter_mut().for_each(|value| *value /= length);
        }
        Ok(vectors)
    }
}

fn model_directory(state: &Path) -> PathBuf {
    state
        .join("models")
        .join(format!("multilingual-e5-small-{MODEL_REVISION}"))
}

fn initialize_runtime(state: &Path, installed_only: bool) -> Result<()> {
    let mut runtime = RUNTIME
        .lock()
        .map_err(|_| "Embedding runtime initialization failed")?;
    if runtime.is_some() {
        return Ok(());
    }
    let installed = state.join("models").join(runtime_filename());
    let path = if installed_only {
        if !io::file(&installed)? {
            return Err("The installed ONNX Runtime is unavailable".into());
        }
        installed
    } else {
        match std::env::var_os("ORT_DYLIB_PATH") {
            Some(path) if !path.is_empty() => PathBuf::from(path),
            _ if io::file(&installed)? => installed,
            _ => PathBuf::from(runtime_filename()),
        }
    };
    let library = load_runtime_library(&path, installed_only)
        .map_err(|error| format!("ONNX Runtime 1.22+ is unavailable: {error}"))?;
    let api = unsafe {
        let getter: libloading::Symbol<unsafe extern "system" fn() -> *const ort::sys::OrtApiBase> =
            library
                .get(b"OrtGetApiBase")
                .map_err(|_| "Invalid ONNX Runtime library")?;
        let base = getter().as_ref().ok_or("Invalid ONNX Runtime library")?;
        (base.GetApi)(ort::sys::ORT_API_VERSION)
            .as_ref()
            .ok_or("ONNX Runtime library must support API 22")?
    };
    ort::set_api(*api);
    *runtime = Some(library);
    Ok(())
}

fn load_runtime_library(
    path: &Path,
    _installed_only: bool,
) -> std::result::Result<libloading::Library, libloading::Error> {
    #[cfg(windows)]
    if _installed_only {
        use libloading::os::windows::{
            Library, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32,
        };
        return unsafe {
            Library::load_with_flags(
                path,
                LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32,
            )
        }
        .map(Into::into);
    }
    unsafe { libloading::Library::new(path) }
}

fn check_model_file(path: &Path, size: u64) -> Result<()> {
    if io::file(path)? && fs::metadata(path).map_err(io::err)?.len() == size {
        return Ok(());
    }
    Err(format!(
        "Embedding model file is missing or has an unexpected size: {}",
        path.display()
    ))
}

fn model_files_verified(directory: &Path) -> Result<bool> {
    for ((relative, expected), size) in MODEL_FILES.into_iter().zip(MODEL_FILE_SIZES) {
        let path = directory.join(relative);
        if !io::file(&path)?
            || fs::metadata(&path).map_err(io::err)?.len() != size
            || hash_model_file(&path, size, &mut std::io::sink())? != expected
        {
            return Ok(false);
        }
    }
    Ok(true)
}

fn hash_model_file(path: &Path, expected_size: u64, output: &mut impl Write) -> Result<String> {
    let input = io::options().read(true).open(path).map_err(io::err)?;
    let mut input = input.take(expected_size + 1);
    let mut hash = Sha256::new();
    let mut copied = 0;
    let mut buffer = vec![0_u8; 128 * 1024];
    loop {
        let read = input.read(&mut buffer).map_err(io::err)?;
        if read == 0 {
            break;
        }
        copied += read as u64;
        output.write_all(&buffer[..read]).map_err(io::err)?;
        hash.update(&buffer[..read]);
    }
    if copied != expected_size {
        return Err("Embedding model file changed during installation".into());
    }
    Ok(format!("{:x}", hash.finalize()))
}
