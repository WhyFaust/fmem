use fmem::{
    embeddings::{runtime_filename, Embeddings, MODEL_FILES, MODEL_ID, MODEL_REVISION},
    memory::io,
    Result,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{Read, Seek, Write},
    path::{Path, PathBuf},
    time::Duration,
};
use tempfile::{Builder, NamedTempFile};
use zip::ZipArchive;

const RUNTIME_VERSION: &str = "1.22.0";
const RUNTIME_LICENSE: &str = "ONNX-Runtime-1.22.0-LICENSE.txt";
const RUNTIME_NOTICES: &str = "ONNX-Runtime-1.22.0-ThirdPartyNotices.txt";
const MAX_ARCHIVE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_RUNTIME_BYTES: u64 = 128 * 1024 * 1024;
const MAX_NOTICE_BYTES: u64 = 1024 * 1024;
const MAX_MODEL_BYTES: u64 = 512 * 1024 * 1024;
const MAX_TOKENIZER_BYTES: u64 = 20 * 1024 * 1024;

const E5_LICENSE: &str = "intfloat/multilingual-e5-small
Model card: https://huggingface.co/intfloat/multilingual-e5-small
Upstream E5 project: https://github.com/microsoft/unilm/tree/master/e5

The MIT License (MIT)

Copyright (c) Microsoft Corporation

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the \"Software\"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED \"AS IS\", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
";

struct RuntimeBundle {
    wheel: &'static str,
    checksum: &'static str,
    library: &'static str,
    library_checksum: &'static str,
}

fn runtime_bundle() -> Result<RuntimeBundle> {
    let linux_library = "onnxruntime/capi/libonnxruntime.so.1.22.0";
    let bundle = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") if cfg!(target_env = "gnu") => RuntimeBundle {
            wheel: "7a/d5/aa83d084d05bc8f6cf8b74b499c77431ffd6b7075c761ec48ec0c161a47f/onnxruntime-1.22.0-cp311-cp311-manylinux_2_27_x86_64.manylinux_2_28_x86_64.whl",
            checksum: "b978aa63a9a22095479c38371a9b359d4c15173cbb164eaad5f2cd27d666aa65",
            library: linux_library,
            library_checksum: "7520bc1b4f649ee8fa956442a66ded303e8fd3b465b0159b7e893bdc87df4e38",
        },
        ("linux", "aarch64") if cfg!(target_env = "gnu") => RuntimeBundle {
            wheel: "3e/8b/22989f6b59bc4ad1324f07a945c80b9ab825f0a581ad7a6064b93716d9b7/onnxruntime-1.22.0-cp311-cp311-manylinux_2_27_aarch64.manylinux_2_28_aarch64.whl",
            checksum: "fef17d665a917866d1f68f09edc98223b9a27e6cb167dec69da4c66484ad12fd",
            library: linux_library,
            library_checksum: "50a440a8a15515eef4f48ae6471c361032cc162769045ebf662099396642259c",
        },
        ("macos", "x86_64" | "aarch64") => RuntimeBundle {
            wheel: "7a/08/c008711d1b92ff1272f4fea0fbee57723171f161d42e5c680625535280af/onnxruntime-1.22.0-cp311-cp311-macosx_13_0_universal2.whl",
            checksum: "8d6725c5b9a681d8fe72f2960c191a96c256367887d076b08466f52b4e0991df",
            library: "onnxruntime/capi/libonnxruntime.1.22.0.dylib",
            library_checksum: "48baaf6c8ef38f6e582736ede27b563dee3b729af9ad497ebd47afd1ff940460",
        },
        ("windows", "x86_64") => RuntimeBundle {
            wheel: "89/a5/1c6c10322201566015183b52ef011dfa932f5dd1b278de8d75c3b948411d/onnxruntime-1.22.0-cp311-cp311-win_amd64.whl",
            checksum: "03d3ef7fb11adf154149d6e767e21057e0e577b947dd3f66190b212528e1db31",
            library: "onnxruntime/capi/onnxruntime.dll",
            library_checksum: "20e5f96903515680d100e4a4a1857392264bc265a749f0734f0770906567b359",
        },
        _ => return Err("Automatic ONNX Runtime installation is unavailable on this platform. Supply a compatible native library with --runtime.".into()),
    };
    Ok(bundle)
}

pub fn install(state: &Path, source: Option<&Path>, runtime: Option<&Path>) -> Result<Value> {
    let state = io::absolute(state)?;
    for ancestor in state.ancestors() {
        if io::directory(&ancestor.join(".obsidian"), false, false)? {
            return Err(
                "Embedding installation requires local state outside Obsidian vaults".into(),
            );
        }
    }
    io::profile_state(&state, true)?;
    let models = state.join("models");
    io::directory(&models, true, true)?;
    let _lock = io::lock_key(
        &format!("fmem-install-{}", io::hash(io::directory_key(&models))),
        None,
    )?;
    let http = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(20))
        .timeout_read(Duration::from_secs(60))
        .timeout(Duration::from_secs(900))
        .redirects(8)
        .https_only(true)
        .try_proxy_from_env(true)
        .user_agent("fmem-model-installer")
        .build();
    let runtime_path = models.join(runtime_filename());
    let pending_runtime = match runtime {
        Some(path) => {
            let path = io::absolute(path)?;
            if !io::file(&path)? {
                return Err("The supplied ONNX Runtime library is unavailable".into());
            }
            let mut file = io::options().read(true).open(path).map_err(io::err)?;
            check_library_format(&mut file)?;
            let mut staged = stage_file(&models)?;
            copy_verified(file, staged.as_file_mut(), MAX_RUNTIME_BYTES, None)?;
            vec![(staged, runtime_path.clone())]
        }
        None => stage_runtime(&http, &models, &runtime_path, runtime_bundle()?)?,
    };

    let installed_model = models.join(format!("multilingual-e5-small-{MODEL_REVISION}"));
    if let Some(source) = source {
        Embeddings::install(&state, &io::absolute(source)?)?;
    } else if !io::directory(&installed_model, false, false)?
        || Embeddings::install(&state, &installed_model).is_err()
    {
        let staging = Builder::new()
            .prefix(".download-model-")
            .tempdir_in(&models)
            .map_err(io::err)?;
        for (relative, checksum) in MODEL_FILES {
            let local = staging.path().join(relative);
            io::directory(
                local.parent().ok_or("Missing model file parent")?,
                true,
                true,
            )?;
            let mut file = io::options()
                .create_new(true)
                .write(true)
                .open(local)
                .map_err(io::err)?;
            let url = format!("https://huggingface.co/{MODEL_ID}/resolve/{MODEL_REVISION}/{relative}?download=true");
            let limit = if relative.ends_with(".onnx") {
                MAX_MODEL_BYTES
            } else {
                MAX_TOKENIZER_BYTES
            };
            download(&http, &url, relative, &mut file, limit, checksum)?;
        }
        Embeddings::install(&state, staging.path())?;
    }
    let license = models.join("multilingual-e5-small-LICENSE.txt");
    if io::read(&license)?.as_deref() != Some(E5_LICENSE) {
        io::write(&license, E5_LICENSE, true, None)?;
    }
    for (staged, path) in pending_runtime {
        io::file(&path)?;
        staged.persist(&path).map_err(|error| {
            format!(
                "Could not publish installed ONNX Runtime file: {}",
                error.error
            )
        })?;
        io::sync_parent(&path)?;
    }
    let mut result = json!({
        "model": MODEL_ID,
        "revision": MODEL_REVISION,
        "model_directory": installed_model,
        "runtime": runtime_path,
        "runtime_source": if runtime.is_some() { "provided" } else { "pypi" },
    });
    if runtime.is_none() {
        result["runtime_version"] = json!(RUNTIME_VERSION);
    }
    Ok(result)
}

fn stage_runtime(
    http: &ureq::Agent,
    models: &Path,
    library: &Path,
    bundle: RuntimeBundle,
) -> Result<Vec<(NamedTempFile, PathBuf)>> {
    let license = models.join(RUNTIME_LICENSE);
    let notices = models.join(RUNTIME_NOTICES);
    if io::file(library)?
        && io::file(&license)?
        && io::file(&notices)?
        && file_checksum(library, MAX_RUNTIME_BYTES)?.as_deref() == Some(bundle.library_checksum)
    {
        return Ok(Vec::new());
    }
    let mut packaged = stage_file(models)?;
    download(
        http,
        &format!("https://files.pythonhosted.org/packages/{}", bundle.wheel),
        "ONNX Runtime",
        packaged.as_file_mut(),
        MAX_ARCHIVE_BYTES,
        bundle.checksum,
    )?;
    let archive = packaged.reopen().map_err(io::err)?;
    let mut archive = ZipArchive::new(archive)
        .map_err(|error| format!("Invalid ONNX Runtime archive: {error}"))?;
    [
        ("onnxruntime/LICENSE", license, MAX_NOTICE_BYTES, None),
        (
            "onnxruntime/ThirdPartyNotices.txt",
            notices,
            MAX_NOTICE_BYTES,
            None,
        ),
        (
            bundle.library,
            library.to_owned(),
            MAX_RUNTIME_BYTES,
            Some(bundle.library_checksum),
        ),
    ]
    .into_iter()
    .map(|(member, path, limit, checksum)| {
        let input = archive
            .by_name(member)
            .map_err(|error| format!("Missing ONNX Runtime archive member {member}: {error}"))?;
        let mut file = stage_file(models)?;
        copy_verified(input, file.as_file_mut(), limit, checksum)?;
        Ok((file, path))
    })
    .collect()
}

fn stage_file(models: &Path) -> Result<NamedTempFile> {
    Builder::new()
        .prefix(".install-")
        .tempfile_in(models)
        .map_err(io::err)
}

fn file_checksum(path: &Path, limit: u64) -> Result<Option<String>> {
    let input = io::options().read(true).open(path).map_err(io::err)?;
    if input.metadata().map_err(io::err)?.len() > limit {
        return Ok(None);
    }
    let mut digest = Sha256::new();
    let read = std::io::copy(&mut input.take(limit + 1), &mut digest).map_err(io::err)?;
    Ok((read <= limit).then(|| format!("{:x}", digest.finalize())))
}

fn download(
    http: &ureq::Agent,
    url: &str,
    description: &str,
    file: &mut File,
    limit: u64,
    checksum: &str,
) -> Result<()> {
    let response = http.get(url).call().map_err(|error| match error {
        ureq::Error::Status(status, _) => {
            format!("Could not download {description}: HTTP {status}")
        }
        ureq::Error::Transport(error) => {
            format!(
                "Could not connect to download {description}: {}",
                error.kind()
            )
        }
    })?;
    copy_verified(response.into_reader(), file, limit, Some(checksum))
        .map_err(|error| format!("Could not verify {description}: {error}"))
}

fn copy_verified(
    input: impl Read,
    destination: &mut File,
    limit: u64,
    expected_checksum: Option<&str>,
) -> Result<()> {
    let mut input = input.take(limit + 1);
    let mut digest = Sha256::new();
    let mut buffer = vec![0; 128 * 1024];
    let mut length = 0;
    loop {
        let count = input.read(&mut buffer).map_err(io::err)?;
        if count == 0 {
            break;
        }
        length += count as u64;
        if length > limit {
            return Err("Install file exceeds its size limit".into());
        }
        digest.update(&buffer[..count]);
        destination.write_all(&buffer[..count]).map_err(io::err)?;
    }
    if length == 0 {
        return Err("Install file is empty".into());
    }
    if expected_checksum.is_some_and(|expected| format!("{:x}", digest.finalize()) != expected) {
        return Err("Install file checksum mismatch".into());
    }
    destination.sync_all().map_err(io::err)
}

fn check_library_format(file: &mut File) -> Result<()> {
    let mut magic = [0_u8; 4];
    file.read_exact(&mut magic)
        .map_err(|_| "The supplied ONNX Runtime library is incomplete")?;
    let valid = match std::env::consts::OS {
        "windows" => magic.starts_with(b"MZ"),
        "macos" => matches!(
            u32::from_be_bytes(magic),
            0xcafebabe | 0xcafebabf | 0xfeedfacf | 0xcffaedfe
        ),
        _ => magic == *b"\x7fELF",
    };
    if !valid {
        return Err("The supplied ONNX Runtime library has an unexpected native format".into());
    }
    file.rewind().map_err(io::err)
}
