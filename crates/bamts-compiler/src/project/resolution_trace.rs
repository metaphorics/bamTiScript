//! Upstream-compatible module-resolution trace rendering.

use super::{
    ProjectRoot,
    resolution::{
        ModuleResolutionKind, ResolutionError, ResolutionMode, ResolutionTraceStep, ResolvedModule,
    },
};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

/// Resolution features whose upstream trace templates are not emitted yet.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum UnsupportedTraceKind {
    /// Directory/package lookups (`package.json`, `index.*`) are not rendered.
    DirectoryPackage,
    /// `#imports` package redirects are not resolved by this tracer.
    PackageImport,
    /// Bare package specifiers are not resolved by this tracer.
    PackageName,
    /// `paths` mappings are not rendered.
    PathsMapping,
}

/// Ordered `traceResolution` lines collected across one compilation.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ResolutionTraceLog {
    lines: Vec<String>,
    unsupported: BTreeSet<UnsupportedTraceKind>,
}

impl ResolutionTraceLog {
    #[must_use]
    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    #[must_use]
    pub fn unsupported(&self) -> &BTreeSet<UnsupportedTraceKind> {
        &self.unsupported
    }

    pub(crate) fn record_module(
        &mut self,
        root: &ProjectRoot,
        importer: &Path,
        specifier: &str,
        strategy: (ModuleResolutionKind, ResolutionMode),
        cached: bool,
        result: &Result<ResolvedModule, ResolutionError>,
    ) {
        if !(specifier.starts_with("./") || specifier.starts_with("../")) {
            self.unsupported.insert(if specifier.starts_with('#') {
                UnsupportedTraceKind::PackageImport
            } else {
                UnsupportedTraceKind::PackageName
            });
            return;
        }

        let directory = importer.parent().unwrap_or_else(|| root.path());
        let Ok(candidate_location) = root.resolve_from(directory, specifier) else {
            return;
        };
        let resolved_parent = result
            .as_ref()
            .ok()
            .and_then(|resolved| resolved.path().parent().map(Path::to_path_buf));
        let extension = candidate_location
            .extension()
            .and_then(|value| value.to_str());

        self.lines.push(format!(
            "======== Resolving module '{specifier}' from '{}'. ========",
            display_path(importer)
        ));
        if cached {
            self.lines.push(format!(
                "Resolution for module '{specifier}' was found in cache from location '{}'.",
                display_path(directory)
            ));
        } else {
            self.lines.push(format!(
                "Explicitly specified module resolution kind: '{}'.",
                strategy.0.trace_name()
            ));
            self.lines.push(format!(
                "Resolving in CJS mode with conditions '{}', 'types'.",
                strategy.1.as_str()
            ));
            self.lines.push(format!(
                "Loading module as file / folder, candidate module location '{}', target file types: TypeScript, JavaScript, Declaration, JSON.",
                display_path(&candidate_location)
            ));
            self.render_candidates(&candidate_location, extension, result);
        }

        match result {
            Ok(resolved) => self.lines.push(format!(
                "======== Module name '{specifier}' was successfully resolved to '{}'. ========",
                display_path(resolved.path())
            )),
            Err(_) => self.lines.push(format!(
                "======== Module name '{specifier}' was not resolved. ========"
            )),
        }
        if resolved_parent.is_some_and(|parent| parent == candidate_location) {
            // The specifier resolved to a directory package; only its file-level
            // probes are rendered above and the package walk stays unrendered.
            self.unsupported
                .insert(UnsupportedTraceKind::DirectoryPackage);
        }
        if extension.is_none() && !result.is_ok() {
            // A failed extensionless lookup continues into directory probes that
            // this tracer does not render; report the gap instead of truncating.
            self.unsupported
                .insert(UnsupportedTraceKind::DirectoryPackage);
        }
    }

    fn render_candidates(
        &mut self,
        requested: &Path,
        extension: Option<&str>,
        result: &Result<ResolvedModule, ResolutionError>,
    ) {
        let trace = match result {
            Ok(resolved) => Some(resolved.trace()),
            Err(error) => error.trace(),
        };
        let Some(trace) = trace else { return };
        let probes = trace
            .steps()
            .iter()
            .filter_map(|step| match step {
                ResolutionTraceStep::Candidate { path, exists } => Some((path.clone(), *exists)),
                ResolutionTraceStep::PathsMatch { .. } => {
                    self.unsupported.insert(UnsupportedTraceKind::PathsMapping);
                    None
                }
                _ => None,
            })
            .collect::<Vec<_>>();

        if let Some(extension) = extension {
            self.lines.push(format!(
                "File name '{}' has a '.{extension}' extension - stripping it.",
                display_path(requested)
            ));
        }
        for candidate in upstream_candidates(requested, extension) {
            let Some(exists) = probes
                .iter()
                .find_map(|(path, exists)| (path == &candidate).then_some(*exists))
            else {
                continue;
            };
            self.lines.push(if exists {
                format!(
                    "File '{}' exists - use it as a name resolution result.",
                    display_path(&candidate)
                )
            } else {
                format!("File '{}' does not exist.", display_path(&candidate))
            });
            if exists {
                break;
            }
        }
    }
}

/// Upstream Bundler-mode probe order for the file-level candidates of one stem.
fn upstream_candidates(requested: &Path, extension: Option<&str>) -> Vec<PathBuf> {
    let extensions: &[&str] = match extension {
        None => &["ts", "tsx", "d.ts", "js", "jsx"],
        Some("js") => &["ts", "tsx", "d.ts", "js"],
        Some("jsx") => &["tsx", "d.ts", "jsx"],
        Some("mjs") => &["mts", "d.mts", "mjs"],
        Some("cjs") => &["cts", "d.cts", "cjs"],
        Some("ts" | "tsx" | "mts" | "cts") => &[extension.expect("extension matched an arm")],
        Some("json") => &["json"],
        Some(_) => &[],
    };
    extensions
        .iter()
        .map(|extension| requested.with_extension(extension))
        .collect()
}

/// Renders a path the way upstream traces do: forward slashes, lowercased drive.
fn display_path(path: &Path) -> String {
    let mut value = path.to_string_lossy().replace('\\', "/");
    if value.as_bytes().get(1) == Some(&b':') {
        value.make_ascii_lowercase();
    }
    value
}
