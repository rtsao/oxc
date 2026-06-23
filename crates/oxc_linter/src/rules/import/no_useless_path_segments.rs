use std::path::Path;

use oxc_ast::{AstKind, ast::Expression};
use oxc_diagnostics::OxcDiagnostic;
use oxc_macros::declare_oxc_lint;
use oxc_span::{Span, VALID_EXTENSIONS};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    AstNode, ModuleRecord,
    context::LintContext,
    rule::{DefaultRuleConfig, Rule},
};

fn no_useless_path_segments_diagnostic(
    span: Span,
    import_path: &str,
    proposed: &str,
) -> OxcDiagnostic {
    OxcDiagnostic::warn(format!(
        "Useless path segments for \"{import_path}\", should be \"{proposed}\""
    ))
    .with_label(span)
}

fn report_path_segments(ctx: &LintContext<'_>, span: Span, import_path: &str, proposed: &str) {
    ctx.diagnostic(no_useless_path_segments_diagnostic(span, import_path, proposed));
}

/// <https://github.com/import-js/eslint-plugin-import/blob/v2.31.0/docs/rules/no-useless-path-segments.md>
#[derive(Default, Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
pub struct NoUselessPathSegments {
    no_useless_index: bool,
}

declare_oxc_lint!(
    /// ### What it does
    ///
    /// Prevents unnecessary path segments in import statements.
    ///
    /// ### Why is this bad?
    ///
    /// Useless path segments like `./`, `../`, or redundant directory navigation
    /// make imports harder to read and maintain.
    ///
    /// ### Examples
    ///
    /// Examples of **incorrect** code for this rule:
    /// ```javascript
    /// import foo from './../bar';      // should be '../bar'
    /// import bar from './foo/../bar';  // should be './bar'
    /// import baz from './';            // should be '.'
    /// import qux from './deep//a';     // should be './deep/a'
    ///
    /// // With noUselessIndex option
    /// import x from './foo/index.js';  // should be './foo' or './foo/'
    /// ```
    ///
    /// Examples of **correct** code for this rule:
    /// ```javascript
    /// import foo from '../bar';
    /// import bar from './bar';
    /// import baz from '.';
    /// import qux from './deep/a';
    /// import external from 'lodash';
    /// ```
    NoUselessPathSegments,
    import,
    pedantic,
    config = NoUselessPathSegments,
    version = "next",
    short_description = "Disallow unnecessary path segments in import statements.",
);

impl Rule for NoUselessPathSegments {
    fn from_configuration(value: serde_json::Value) -> Result<Self, serde_json::error::Error> {
        serde_json::from_value::<DefaultRuleConfig<Self>>(value).map(DefaultRuleConfig::into_inner)
    }

    fn run<'a>(&self, node: &AstNode<'a>, ctx: &LintContext<'a>) {
        let AstKind::ImportExpression(import_expr) = node.kind() else {
            return;
        };
        let Some(import_path) = static_module_name(&import_expr.source) else {
            return;
        };
        if !import_path.starts_with('.') {
            return;
        }

        if let Some(proposed) = self.check_path(import_path, ctx.file_path(), ctx.module_record()) {
            report_path_segments(ctx, import_expr.span, import_path, &proposed);
        }
    }

    fn run_once(&self, ctx: &LintContext<'_>) {
        let module_record = ctx.module_record();

        for (import_path, requested_modules) in &module_record.requested_modules {
            if !import_path.starts_with('.') {
                continue;
            }

            let Some(proposed) = self.check_path(import_path, ctx.file_path(), module_record)
            else {
                continue;
            };

            for requested_module in requested_modules {
                report_path_segments(ctx, requested_module.span, import_path, &proposed);
            }
        }
    }
}

fn static_module_name<'a>(source: &'a Expression<'a>) -> Option<&'a str> {
    match source {
        Expression::StringLiteral(str_lit) => Some(str_lit.value.as_str()),
        Expression::TemplateLiteral(tpl) if tpl.is_no_substitution_template() => {
            tpl.quasis.first().map(|quasi| quasi.value.raw.as_str())
        }
        _ => None,
    }
}

impl NoUselessPathSegments {
    /// Check if a path component is an index file.
    ///
    /// Returns `true` for "index" or "index.{ext}" where ext is in VALID_EXTENSIONS.
    /// Avoids string allocations by using string slicing.
    ///
    /// # Examples
    ///
    /// assert!(is_index_filename("index"));
    /// assert!(is_index_filename("index.js"));
    /// assert!(is_index_filename("index.ts"));
    /// assert!(!is_index_filename("index.foo"));
    /// assert!(!is_index_filename("notindex"));
    fn is_index_filename(name: &str) -> bool {
        name == "index" || Self::is_index_with_extension(name)
    }

    /// Check if name is "index.{ext}" where ext is a valid JavaScript/TypeScript extension.
    #[inline]
    fn is_index_with_extension(name: &str) -> bool {
        name.strip_prefix("index.").is_some_and(|ext| VALID_EXTENSIONS.contains(&ext))
    }

    /// Check if a resolved path points to an index file.
    ///
    /// This indicates that the import resolved to a directory,
    /// which means we might be able to suggest removing the "/index" suffix.
    ///
    /// # Examples
    ///
    /// /path/to/bar/index.js → true (can suggest "./bar")
    /// /path/to/foo.js → false (explicit file, can't simplify)
    fn resolved_to_index_file(resolved_path: &Path) -> bool {
        resolved_path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(Self::is_index_filename)
    }

    /// Helper to optionally remove useless index if the feature is enabled.
    /// Reduces code duplication by wrapping the common pattern.
    fn maybe_remove_index(&self, path: String, module_record: &ModuleRecord) -> String {
        if self.no_useless_index {
            Self::remove_useless_index(&path, module_record).unwrap_or(path)
        } else {
            path
        }
    }

    /// Check if a trailing slash is meaningful (i.e., changes resolution).
    ///
    /// Trailing slashes can be meaningful when:
    /// - `./bar/` resolves to `bar/index.js` (directory)
    /// - `./bar` resolves to `bar.js` (file)
    ///
    /// Returns `true` if the slash should be kept, `false` if it can be removed.
    fn is_trailing_slash_meaningful(
        import_path: &str,
        without_slash: &str,
        module_record: &ModuleRecord,
    ) -> bool {
        let with_slash = module_record.get_loaded_module(import_path);
        let without = module_record.get_loaded_module(without_slash);

        match (with_slash, without) {
            (Some(slashed), Some(unslashed)) => {
                slashed.resolved_absolute_path != unslashed.resolved_absolute_path
            }
            (Some(module), None) => Self::resolved_to_index_file(&module.resolved_absolute_path),
            (None, Some(_)) => true,
            (None, None) => false,
        }
    }

    fn check_path(
        &self,
        import_path: &str,
        current_file: &Path,
        module_record: &ModuleRecord,
    ) -> Option<String> {
        if let Some(normalized) = self.try_normalize(import_path, module_record) {
            return Some(normalized);
        }

        if self.no_useless_index
            && let Some(without_index) = Self::remove_useless_index(import_path, module_record)
        {
            return Some(without_index);
        }

        if import_path.starts_with("./") {
            return None;
        }

        self.try_simplify_by_resolution(import_path, current_file, module_record)
    }

    /// Try to normalize the path textually (e.g., "./foo/../bar" → "./bar").
    ///
    /// This also handles special cases like trailing slashes and checks whether
    /// normalization would break resolution.
    fn try_normalize(&self, import_path: &str, module_record: &ModuleRecord) -> Option<String> {
        if !may_need_normalization(import_path) {
            return None;
        }

        let normalized = normalize_path(import_path);

        if normalized == import_path {
            return None; // Already normalized
        }

        if import_path.ends_with('/') && !normalized.ends_with('/') {
            let without_slash = import_path.trim_end_matches('/');
            if normalized == without_slash {
                if Self::is_trailing_slash_meaningful(import_path, without_slash, module_record) {
                    return None;
                }

                return Some(self.maybe_remove_index(normalized, module_record));
            }
        }

        if Self::resolves_to_different_module(import_path, &normalized, module_record) {
            return None;
        }

        Some(self.maybe_remove_index(normalized, module_record))
    }

    fn resolves_to_different_module(
        import_path: &str,
        candidate: &str,
        module_record: &ModuleRecord,
    ) -> bool {
        let Some(original) = module_record.get_loaded_module(import_path) else {
            return false;
        };
        let Some(candidate) = module_record.get_loaded_module(candidate) else {
            return false;
        };

        original.resolved_absolute_path != candidate.resolved_absolute_path
    }

    /// Try resolution-based simplification.
    ///
    /// This tries to find simpler paths that resolve to the same module.
    /// For example, from a file at "files/foo.js", importing "../files/foo"
    /// can be simplified to "./foo".
    ///
    /// We iterate through progressively simpler paths rather than calculating
    /// directly (like the JS implementation) because module_record may not have
    /// complete resolution info for all cases.
    fn try_simplify_by_resolution(
        &self,
        import_path: &str,
        current_file: &Path,
        module_record: &ModuleRecord,
    ) -> Option<String> {
        // For ESM: use module_record exclusively (no filesystem checks needed)
        let original_module = module_record.get_loaded_module(import_path)?;

        // Special case: Check if the import resolves to the current file (self-import)
        // E.g., from foo.js, import '../files/foo.js' → suggests './foo.js'
        if original_module.resolved_absolute_path == module_record.resolved_absolute_path {
            return self.suggest_self_import_path(import_path, current_file, module_record);
        }

        // General case: Try progressively simpler paths
        self.try_remove_parent_directories(
            import_path,
            original_module.resolved_absolute_path.as_path(),
            module_record,
        )
    }

    /// Suggest the shortest path for a self-import.
    ///
    /// When a module imports itself, the shortest path is always "./filename"
    /// (with extension if the original import had one).
    fn suggest_self_import_path(
        &self,
        import_path: &str,
        current_file: &Path,
        module_record: &ModuleRecord,
    ) -> Option<String> {
        let file_name = current_file.file_stem()?.to_str()?;
        let extension = current_file.extension().and_then(|e| e.to_str());

        let suggested = if let Some(ext) = extension {
            // Preserve extension only if original import had it
            if import_path.ends_with(&format!(".{ext}")) {
                format!("./{file_name}.{ext}")
            } else {
                format!("./{file_name}")
            }
        } else {
            format!("./{file_name}")
        };

        Some(self.maybe_remove_index(suggested, module_record))
    }

    /// Try removing unnecessary parent directory traversals.
    ///
    /// For paths with ".." segments, try progressively removing them to find
    /// simpler paths that resolve to the same location.
    fn try_remove_parent_directories(
        &self,
        import_path: &str,
        resolved_path: &Path,
        module_record: &ModuleRecord,
    ) -> Option<String> {
        let import_segments: Vec<&str> = import_path.split('/').collect();
        let leading_parents =
            import_segments.iter().take_while(|segment| **segment == "..").count();
        if leading_parents == 0 {
            return None;
        }

        for parents_to_remove in 1..=leading_parents {
            let remaining_parents = leading_parents - parents_to_remove;

            let mut new_segments: Vec<&str> =
                std::iter::repeat_n("..", remaining_parents).collect();

            for seg in import_segments.iter().skip(leading_parents + parents_to_remove) {
                new_segments.push(*seg);
            }

            let suggested = if new_segments.is_empty() {
                ".".to_string()
            } else {
                let joined = new_segments.join("/");
                if joined.starts_with("..") { joined } else { format!("./{joined}") }
            };

            if let Some(suggested_module) = module_record.get_loaded_module(&suggested)
                && suggested_module.resolved_absolute_path == resolved_path
            {
                return Some(self.maybe_remove_index(suggested, module_record));
            }
        }

        None
    }

    /// Remove useless "/index" or "/index.{ext}" suffixes from import paths.
    ///
    /// When `noUselessIndex` is enabled, paths like "./bar/index.js" can be
    /// simplified to "./bar" since Node.js resolution handles this automatically.
    ///
    /// # Examples
    ///
    /// ```javascript
    /// import './bar/index.js'  // → './bar'
    /// import './bar/index'     // → './bar'
    /// import './index'         // → '.'
    /// import '../index.ts'     // → '..'
    /// ```
    fn remove_useless_index(import_path: &str, module_record: &ModuleRecord) -> Option<String> {
        let has_index_suffix = import_path.ends_with("/index")
            || import_path
                .rsplit_once("/index.")
                .is_some_and(|(_, ext)| VALID_EXTENSIONS.contains(&ext));

        if !has_index_suffix {
            // Check bare "index" cases: ./index, ../index, ./index.ext, ../index.ext
            return Self::try_remove_bare_index(import_path, module_record);
        }

        let parent = import_path.rsplit_once('/').map(|(p, _)| p)?;
        let parent = if parent.is_empty() { "." } else { parent };

        module_record.get_loaded_module(import_path)?;

        if module_record.get_loaded_module(parent).is_some() {
            return Some(parent.to_string());
        }

        let parent_with_slash = format!("{parent}/");
        if module_record.get_loaded_module(&parent_with_slash).is_some() {
            Some(parent_with_slash)
        } else {
            Some(parent.to_string())
        }
    }

    fn bare_index_parent(import_path: &str) -> Option<&'static str> {
        if import_path == "./index"
            || import_path
                .strip_prefix("./index.")
                .is_some_and(|ext| VALID_EXTENSIONS.contains(&ext))
        {
            Some(".")
        } else if import_path == "../index"
            || import_path
                .strip_prefix("../index.")
                .is_some_and(|ext| VALID_EXTENSIONS.contains(&ext))
        {
            Some("..")
        } else {
            None
        }
    }

    /// Try to remove bare "index" references (e.g., "./index" → ".").
    ///
    /// Handles special cases where the import is directly to an index file
    /// without a parent directory in the path.
    fn try_remove_bare_index(import_path: &str, module_record: &ModuleRecord) -> Option<String> {
        let parent = Self::bare_index_parent(import_path)?;

        if let (Some(index_mod), Some(parent_mod)) =
            (module_record.get_loaded_module(import_path), module_record.get_loaded_module(parent))
            && index_mod.resolved_absolute_path == parent_mod.resolved_absolute_path
        {
            return Some(parent.to_string());
        }

        None
    }
}

#[inline]
fn may_need_normalization(path: &str) -> bool {
    path.ends_with('/')
        || path.ends_with("/.")
        || path.ends_with("/..")
        || path.contains("//")
        || path.contains("/./")
        || path.contains("/../")
}

/// Normalize a relative path by resolving "." and ".." components.
///
/// This performs textual normalization without filesystem access:
/// - Removes redundant "./" segments (except leading one)
/// - Resolves "../" against preceding path segments
/// - Removes double slashes
/// - Removes trailing slashes
///
/// # Examples
///
/// normalize_path("./foo/../bar") → "./bar"
/// normalize_path("./deep//a")    → "./deep/a"
/// normalize_path("./../foo")     → "../foo"
/// normalize_path("./")           → "."
/// normalize_path("./bar/")       → "./bar"
fn normalize_path(path: &str) -> String {
    let starts_with_dot = path.starts_with("./");
    let mut segments = Vec::new();

    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                if segments.last().is_some_and(|last| *last != "..") {
                    segments.pop();
                } else {
                    segments.push("..");
                }
            }
            segment => segments.push(segment),
        }
    }

    if segments.is_empty() {
        return if path.starts_with("..") { "..".to_string() } else { ".".to_string() };
    }

    let normalized = segments.join("/");
    if starts_with_dot && !normalized.starts_with("..") {
        format!("./{normalized}")
    } else {
        normalized
    }
}

#[test]
fn test_normalize_path() {
    assert_eq!(normalize_path("./deep//a"), "./deep/a");
    assert_eq!(normalize_path("./../foo"), "../foo");
    assert_eq!(normalize_path("./foo/./bar"), "./foo/bar");
    assert_eq!(normalize_path("./"), ".");
    assert_eq!(normalize_path("../"), "..");
    assert_eq!(normalize_path("./bar/"), "./bar"); // Remove trailing slash
    assert_eq!(normalize_path("./test-module/"), "./test-module"); // Remove trailing slash
}

#[test]
fn test() {
    use crate::tester::Tester;
    use serde_json::json;

    let pass = vec![
        // CommonJS modules are ignored.
        ("require('./../files/malformed.js')", None),
        // ES modules with default options
        ("import './malformed.js'", None),
        ("import './test-module'", None),
        ("import './bar/'", None),
        ("import './'", None),
        ("import '../'", None),
        ("import '.'", None),
        ("import '..'", None),
        ("import fs from 'fs'", None),
        // ES modules + noUselessIndex
        ("import '../index'", None), // noUselessIndex is false by default
        ("import '../my-custom-index'", Some(json!([{"noUselessIndex": true}]))),
        ("import './bar.js'", Some(json!([{"noUselessIndex": true}]))), // ./bar/index.js exists
        ("import './bar'", Some(json!([{"noUselessIndex": true}]))),
        ("import './bar/'", Some(json!([{"noUselessIndex": true}]))), // ./bar.js exists
        ("import './malformed.js'", Some(json!([{"noUselessIndex": true}]))), // ./malformed directory does not exist
        ("import './malformed'", Some(json!([{"noUselessIndex": true}]))), // ./malformed directory does not exist
        ("import './importType'", Some(json!([{"noUselessIndex": true}]))), // ./importType.js does not exist
        ("import('.')", None),
        ("import('..')", None),
        ("import('fs').then(function(fs) {})", None),
    ];

    let fail = vec![
        // ES modules
        ("import './../files/malformed.js'", None),
        ("import './../files/malformed'", None),
        ("import './deep//a'", None),
        // ES modules + noUselessIndex
        ("import './bar/index.js'", Some(json!([{"noUselessIndex": true}]))), // ./bar.js exists
        ("import './bar/index'", Some(json!([{"noUselessIndex": true}]))),    // ./bar.js exists
        ("import './index'", Some(json!([{"noUselessIndex": true}]))),
        ("import '../index'", Some(json!([{"noUselessIndex": true}]))),
        ("import '../index.js'", Some(json!([{"noUselessIndex": true}]))),
        ("import('./')", None),
        ("import('../')", None),
        ("import('./deep//a')", None),
    ];

    Tester::new(NoUselessPathSegments::NAME, NoUselessPathSegments::PLUGIN, pass, fail)
        .with_import_plugin(true)
        .change_rule_path("malformed.js")
        .test_and_snapshot();
}
