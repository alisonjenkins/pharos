//! EPUB 2 and 3: the container, the package document, and the table of
//! contents (EPUB 3 `nav`, else the EPUB 2 NCX).

use crate::archive::{Archive, ArchiveError};
use crate::path::{self, Href};
use crate::section::Target;
use crate::xml::{self, Element, XmlError};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Metadata {
    pub title: Option<String>,
    pub creators: Vec<String>,
    pub language: Option<String>,
    pub identifier: Option<String>,
    pub publisher: Option<String>,
    /// Archive path of the cover image, when the package names one.
    pub cover: Option<String>,
    /// `rtl` when the spine says pages progress right to left.
    pub right_to_left: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpineItem {
    pub path: String,
    pub media_type: String,
    /// `false` for `linear="no"` items (pop-up notes, answer keys).
    pub linear: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TocEntry {
    pub title: String,
    /// `None` for a heading with no link of its own.
    pub target: Option<Target>,
    pub children: Vec<TocEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ManifestItem {
    pub id: String,
    pub path: String,
    pub media_type: String,
    pub properties: String,
}

pub(crate) struct Package {
    pub metadata: Metadata,
    pub manifest: Vec<ManifestItem>,
    pub spine: Vec<SpineItem>,
    pub toc: Vec<TocEntry>,
}

#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    #[error(transparent)]
    Archive(#[from] ArchiveError),
    #[error("META-INF/container.xml: {0}")]
    Container(#[source] XmlError),
    #[error("META-INF/container.xml names no package document")]
    NoPackage,
    #[error("package document {path}: {source}")]
    Package {
        path: String,
        #[source]
        source: XmlError,
    },
    #[error("package document {path} has nothing in its reading order")]
    EmptySpine { path: String },
}

pub(crate) fn open(archive: &dyn Archive) -> Result<Package, OpenError> {
    let container =
        xml::parse_bytes(&archive.read("META-INF/container.xml")?).map_err(OpenError::Container)?;
    let opf_path = container
        .descendants()
        .into_iter()
        .filter(|e| e.is("rootfile"))
        .find(|e| {
            e.attr("media-type")
                .is_none_or(|t| t == "application/oebps-package+xml")
        })
        .and_then(|e| e.attr("full-path"))
        .and_then(|p| match path::resolve("", p) {
            Href::Internal { path, .. } => Some(path),
            _ => None,
        })
        .ok_or(OpenError::NoPackage)?;
    let opf = xml::parse_bytes(&archive.read(&opf_path)?).map_err(|source| OpenError::Package {
        path: opf_path.clone(),
        source,
    })?;

    let manifest = manifest(&opf, &opf_path);
    let spine_el = opf.elements().find(|e| e.is("spine"));
    let spine: Vec<SpineItem> = spine_el
        .map(|s| {
            s.elements()
                .filter(|e| e.is("itemref"))
                .filter_map(|r| {
                    let item = manifest
                        .iter()
                        .find(|m| Some(m.id.as_str()) == r.attr("idref"))?;
                    Some(SpineItem {
                        path: item.path.clone(),
                        media_type: item.media_type.clone(),
                        linear: r.attr("linear") != Some("no"),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    if spine.is_empty() {
        return Err(OpenError::EmptySpine { path: opf_path });
    }

    let mut metadata = metadata(&opf, &manifest);
    metadata.right_to_left =
        spine_el.and_then(|s| s.attr("page-progression-direction")) == Some("rtl");

    let spine_of = |p: &str| spine.iter().position(|s| s.path == p);
    let toc = nav_toc(archive, &manifest, &spine_of)
        .or_else(|| ncx_toc(archive, &manifest, spine_el, &spine_of))
        .unwrap_or_default();

    Ok(Package {
        metadata,
        manifest,
        spine,
        toc,
    })
}

fn manifest(opf: &Element, opf_path: &str) -> Vec<ManifestItem> {
    let Some(m) = opf.elements().find(|e| e.is("manifest")) else {
        return Vec::new();
    };
    m.elements()
        .filter(|e| e.is("item"))
        .filter_map(|e| {
            let Href::Internal { path, .. } = path::resolve(opf_path, e.attr("href")?) else {
                return None;
            };
            Some(ManifestItem {
                id: e.attr("id")?.to_owned(),
                path,
                media_type: e.attr("media-type").unwrap_or_default().to_owned(),
                properties: e.attr("properties").unwrap_or_default().to_owned(),
            })
        })
        .collect()
}

fn metadata(opf: &Element, manifest: &[ManifestItem]) -> Metadata {
    let Some(md) = opf.elements().find(|e| e.is("metadata")) else {
        return Metadata::default();
    };
    let all = md.descendants();
    let first = |name: &str| {
        all.iter()
            .find(|e| e.is(name))
            .map(|e| e.text().trim().to_owned())
            .filter(|t| !t.is_empty())
    };
    let cover = manifest
        .iter()
        .find(|m| m.properties.split_whitespace().any(|p| p == "cover-image"))
        .or_else(|| {
            let id = all
                .iter()
                .find(|e| e.is("meta") && e.attr("name") == Some("cover"))
                .and_then(|e| e.attr("content"))?;
            manifest.iter().find(|m| m.id == id)
        })
        .map(|m| m.path.clone());
    Metadata {
        title: first("title"),
        creators: all
            .iter()
            .filter(|e| e.is("creator"))
            .map(|e| e.text().trim().to_owned())
            .filter(|t| !t.is_empty())
            .collect(),
        language: first("language"),
        identifier: first("identifier"),
        publisher: first("publisher"),
        cover,
        right_to_left: false,
    }
}

fn target(base: &str, href: &str, spine_of: &dyn Fn(&str) -> Option<usize>) -> Option<Target> {
    match path::resolve(base, href) {
        Href::Internal { path, fragment } => Some(Target {
            spine: spine_of(&path)?,
            fragment,
        }),
        _ => None,
    }
}

fn nav_toc(
    archive: &dyn Archive,
    manifest: &[ManifestItem],
    spine_of: &dyn Fn(&str) -> Option<usize>,
) -> Option<Vec<TocEntry>> {
    let item = manifest
        .iter()
        .find(|m| m.properties.split_whitespace().any(|p| p == "nav"))?;
    let doc = xml::parse_html_bytes(&archive.read(&item.path).ok()?).ok()?;
    let navs: Vec<&Element> = doc
        .descendants()
        .into_iter()
        .filter(|e| e.is("nav"))
        .collect();
    let nav = navs
        .iter()
        .find(|n| {
            n.attr("type")
                .is_some_and(|t| t.split_whitespace().any(|t| t == "toc"))
        })
        .or_else(|| navs.first())?;
    let ol = nav.descendants().into_iter().find(|e| e.is("ol"))?;
    let entries = nav_list(ol, &item.path, spine_of);
    (!entries.is_empty()).then_some(entries)
}

fn nav_list(ol: &Element, base: &str, spine_of: &dyn Fn(&str) -> Option<usize>) -> Vec<TocEntry> {
    ol.elements()
        .filter(|li| li.is("li"))
        .filter_map(|li| {
            let label = li.elements().find(|e| e.is("a") || e.is("span"))?;
            let title = collapse(&label.text());
            let target = label.attr("href").and_then(|h| target(base, h, spine_of));
            let children = li
                .elements()
                .find(|e| e.is("ol"))
                .map(|o| nav_list(o, base, spine_of))
                .unwrap_or_default();
            Some(TocEntry {
                title,
                target,
                children,
            })
        })
        .collect()
}

fn ncx_toc(
    archive: &dyn Archive,
    manifest: &[ManifestItem],
    spine: Option<&Element>,
    spine_of: &dyn Fn(&str) -> Option<usize>,
) -> Option<Vec<TocEntry>> {
    let item = spine
        .and_then(|s| s.attr("toc"))
        .and_then(|id| manifest.iter().find(|m| m.id == id))
        .or_else(|| {
            manifest
                .iter()
                .find(|m| m.media_type == "application/x-dtbncx+xml")
        })?;
    let doc = xml::parse_bytes(&archive.read(&item.path).ok()?).ok()?;
    let map = doc.find("navMap")?;
    Some(ncx_points(map, &item.path, spine_of))
}

fn ncx_points(
    parent: &Element,
    base: &str,
    spine_of: &dyn Fn(&str) -> Option<usize>,
) -> Vec<TocEntry> {
    parent
        .elements()
        .filter(|e| e.is("navPoint"))
        .map(|p| TocEntry {
            title: p
                .elements()
                .find(|e| e.is("navLabel"))
                .map(|l| collapse(&l.text()))
                .unwrap_or_default(),
            target: p
                .elements()
                .find(|e| e.is("content"))
                .and_then(|c| c.attr("src"))
                .and_then(|s| target(base, s, spine_of)),
            children: ncx_points(p, base, spine_of),
        })
        .collect()
}

fn collapse(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}
