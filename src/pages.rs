//! The HTML pages a job made: reports, charts and tables the Agent page
//! shows next to the answer.
//!
//! The system prompt asks the agent to save each page as a complete HTML
//! document under `pages/` in its workspace. After a successful run the
//! runner reads them (and any `.html` left at the workspace's top level)
//! before the workspace is deleted, and stores them with the answer
//! (`payload.pages` of the `ai` row). The app shows them only in a sandboxed
//! frame with no network, so a page can never reach anything.

use std::path::Path;

use serde::Serialize;
use tracing::warn;

/// The folder in the workspace the agent saves its pages in.
pub const PAGES_DIR: &str = "pages";

/// Most pages kept per answer.
pub const MAX_PAGES: usize = 6;
/// Largest page kept, in bytes.
pub const MAX_PAGE_BYTES: u64 = 1_500_000;
/// Most bytes of pages kept per answer (the API answers in one Lambda
/// response of at most 6 MB).
pub const MAX_TOTAL_BYTES: usize = 3_000_000;

/// One page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Page {
    /// Its file name (`sales.html`).
    pub name: String,
    /// Its `<title>`, else the file name without `.html`.
    pub title: String,
    /// The whole document.
    pub html: String,
}

/// The text of the document's `<title>`, if it has one.
fn title_of(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let start = lower.find("<title")?;
    let open_end = start + lower[start..].find('>')? + 1;
    let close = open_end + lower[open_end..].find("</title>")?;
    let title = html[open_end..close].split_whitespace().collect::<Vec<_>>().join(" ");
    (!title.is_empty()).then(|| {
        title
            .replace("&amp;", "&")
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&#39;", "'")
    })
}

async fn html_files(dir: &Path) -> Vec<(String, std::path::PathBuf)> {
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return Vec::new();
    };
    let mut files = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        let is_html = path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("html") || ext.eq_ignore_ascii_case("htm"));
        let is_file = entry.file_type().await.is_ok_and(|kind| kind.is_file());
        if is_html && is_file {
            files.push((entry.file_name().to_string_lossy().into_owned(), path));
        }
    }
    files.sort();
    files
}

/// The pages in `workspace`: `pages/*.html` first, then top-level `*.html`,
/// each in name order, within [`MAX_PAGES`], [`MAX_PAGE_BYTES`] and
/// [`MAX_TOTAL_BYTES`]. A page that is too big or not UTF-8 is skipped.
pub async fn collect(workspace: &Path) -> Vec<Page> {
    let mut files = html_files(&workspace.join(PAGES_DIR)).await;
    files.extend(html_files(workspace).await);
    let mut pages = Vec::new();
    let mut total = 0;
    for (name, path) in files {
        if pages.len() == MAX_PAGES {
            break;
        }
        let size = tokio::fs::metadata(&path).await.map_or(u64::MAX, |m| m.len());
        if size > MAX_PAGE_BYTES {
            warn!(page = %name, size, "page too big, left out");
            continue;
        }
        let Ok(html) = tokio::fs::read_to_string(&path).await else {
            warn!(page = %name, "page is not UTF-8, left out");
            continue;
        };
        if html.trim().is_empty() || total + html.len() > MAX_TOTAL_BYTES {
            continue;
        }
        total += html.len();
        let title = title_of(&html).unwrap_or_else(|| {
            name.trim_end_matches(".html")
                .trim_end_matches(".htm")
                .replace(['-', '_'], " ")
        });
        pages.push(Page { name, title, html });
    }
    pages
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_come_from_the_document() {
        assert_eq!(
            title_of("<html><head><TITLE lang=en> Sales &amp;\n  quotes </TITLE>").as_deref(),
            Some("Sales & quotes")
        );
        assert_eq!(title_of("<title></title>"), None);
        assert_eq!(title_of("<p>no title</p>"), None);
    }

    #[tokio::test]
    async fn pages_are_collected_in_order_within_limits() {
        let dir = tempfile::tempdir().unwrap();
        let pages_dir = dir.path().join(PAGES_DIR);
        std::fs::create_dir(&pages_dir).unwrap();
        std::fs::write(pages_dir.join("b.html"), "<title>Bee</title><p>b</p>").unwrap();
        std::fs::write(pages_dir.join("a-report.html"), "<p>a</p>").unwrap();
        std::fs::write(pages_dir.join("notes.txt"), "x").unwrap();
        std::fs::write(pages_dir.join("empty.html"), "  ").unwrap();
        std::fs::write(dir.path().join("top.html"), "<p>top</p>").unwrap();
        let big = "x".repeat(usize::try_from(MAX_PAGE_BYTES).unwrap() + 1);
        std::fs::write(pages_dir.join("c-big.html"), big).unwrap();

        let pages = collect(dir.path()).await;
        let names: Vec<_> = pages.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["a-report.html", "b.html", "top.html"]);
        assert_eq!(pages[0].title, "a report");
        assert_eq!(pages[1].title, "Bee");
        assert_eq!(collect(&dir.path().join("missing")).await, Vec::<Page>::new());
    }
}
