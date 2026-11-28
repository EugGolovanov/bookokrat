use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};

use crate::vendored::rustpub;

/// Reuses the URL's EPUB in `library_dir`. An explicit output path must not exist.
pub fn save_website(input: &str, library_dir: &Path, output: Option<&Path>) -> Result<PathBuf> {
    let mut url = url::Url::parse(input).context("expected an HTTP(S) article URL")?;
    ensure!(
        matches!(url.scheme(), "http" | "https") && url.host().is_some(),
        "expected an HTTP(S) article URL"
    );
    ensure!(
        url.username().is_empty() && url.password().is_none(),
        "article URLs must not contain credentials"
    );
    url.set_fragment(None);

    let path = if let Some(output) = output {
        ensure!(
            output
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("epub")),
            "output must have an .epub extension"
        );
        ensure!(!output.try_exists()?, "{} already exists", output.display());
        output.to_owned()
    } else {
        let label = url
            .path_segments()
            .and_then(|mut p| p.rfind(|p| !p.is_empty()))
            .or(url.host_str())
            .unwrap_or("article");
        let slug: String = label
            .chars()
            .take(60)
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let hash = format!("{:x}", md5::compute(url.as_str()));
        let path = library_dir.join(format!("web-{slug}-{}.epub", &hash[..16]));
        if path.is_file() {
            return Ok(path);
        }
        path
    };

    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut partial = tempfile::NamedTempFile::new_in(dir)
        .with_context(|| format!("cannot create an EPUB in {}", dir.display()))?;
    rustpub::convert(&url, partial.as_file_mut()).context("could not convert web article")?;
    partial
        .persist_noclobber(&path)
        .with_context(|| format!("cannot save {}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reuses_saved_article_in_selected_library() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("web-my-article-ea5df35d72d90073.epub");
        std::fs::write(&path, b"saved").unwrap();
        let url = "https://example.org/my-article#section";

        assert_eq!(save_website(url, dir.path(), None).unwrap(), path);
        assert!(save_website(url, dir.path(), Some(&path)).is_err());
        assert_eq!(std::fs::read(path).unwrap(), b"saved");
    }
}
