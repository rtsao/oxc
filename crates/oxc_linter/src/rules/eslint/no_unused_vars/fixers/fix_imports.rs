use oxc_ast::ast::{ImportDeclaration, ImportDeclarationSpecifier, Statement};
use oxc_span::{GetSpan, Span};

use super::{NoUnusedVars, Symbol, count_whitespace_or_commas};
use crate::{
    context::LintContext,
    fixer::{RuleFix, RuleFixer},
    rule::RuleMeta,
};

impl NoUnusedVars {
    pub(in super::super) fn remove_unused_import_declaration<'a>(
        &self,
        fixer: RuleFixer<'_, 'a>,
        ctx: &LintContext<'a>,
        symbol: &Symbol<'_, 'a>,
        import: &ImportDeclaration<'a>,
    ) -> RuleFix {
        let specifiers = import
            .specifiers
            .as_ref()
            .expect("Found an unused variable in an ImportDeclaration with no specifiers. This should be impossible.");

        debug_assert!(
            !specifiers.is_empty(),
            "Found an unused variable in an ImportDeclaration with no specifiers. This should be impossible."
        );

        let Some(position) = specifiers.iter().position(|s| symbol == s) else {
            debug_assert!(
                false,
                "Could not find matching specifier for symbol in ImportDeclaration."
            );
            return fixer.noop();
        };

        // Every diagnostic in this declaration reports the same fix, which removes all of its
        // unused specifiers at once. Per-specifier fixes would be adjacent to each other, and
        // the fixer skips fixes that touch an already-applied fix, so only some would apply.
        let mut unused: Vec<bool> =
            specifiers.iter().map(|specifier| self.is_unused_specifier(ctx, specifier)).collect();
        unused[position] = true;

        // `import foo, { bar, baz } from 'module';` where all specifiers are unused
        if unused.iter().all(|is_unused| *is_unused) {
            return self.delete_unused_import_declarations(fixer, ctx, import);
        }

        let fixer = fixer.for_multifix();
        let mut fix = fixer.new_fix_with_capacity(unused.iter().filter(|u| **u).count());

        for (specifier, _) in specifiers.iter().zip(&unused).filter(|(_, is_unused)| **is_unused) {
            match specifier {
                ImportDeclarationSpecifier::ImportNamespaceSpecifier(_) => {
                    fix.push(Self::remove_namespace_import(fixer, specifiers, specifier));
                }
                ImportDeclarationSpecifier::ImportDefaultSpecifier(_) => {
                    fix.push(Self::remove_default_import(fixer, specifiers, specifier));
                }
                ImportDeclarationSpecifier::ImportSpecifier(_) => {}
            }
        }

        let named: Vec<_> = specifiers
            .iter()
            .zip(unused.iter().copied())
            .filter(|(s, _)| matches!(s, ImportDeclarationSpecifier::ImportSpecifier(_)))
            .collect();

        if !named.is_empty() && named.iter().all(|(_, is_unused)| *is_unused) {
            fix.push(Self::remove_named_imports_block(fixer, import, specifiers));
        } else {
            for (named_pos, _) in named.iter().enumerate().filter(|(_, (_, is_unused))| *is_unused)
            {
                let followed_by_kept = named[named_pos + 1..].iter().any(|(_, u)| !u);
                fix.push(Self::remove_named_specifier(fixer, &named, named_pos, followed_by_kept));
            }
        }

        fix.with_message("Remove unused import")
    }

    /// Check if a specifier is unused (has no references) and reported by this rule
    fn is_unused_specifier(
        &self,
        ctx: &LintContext<'_>,
        specifier: &ImportDeclarationSpecifier<'_>,
    ) -> bool {
        let specifier_symbol = Symbol::new(ctx, ctx.module_record(), specifier.local().symbol_id());
        !Self::should_skip_symbol(&specifier_symbol)
            && self.is_ignored(&specifier_symbol).is_none()
            && !specifier_symbol.is_exported_binding()
            && !specifier_symbol.has_usages(self)
            && !ctx.disable_directives().contains(Self::NAME, specifier_symbol.span())
    }

    /// Check if a declaration has specifiers and all of them are unused
    fn all_specifiers_unused(&self, ctx: &LintContext<'_>, import: &ImportDeclaration<'_>) -> bool {
        import.specifiers.as_ref().is_some_and(|specifiers| {
            !specifiers.is_empty()
                && specifiers.iter().all(|specifier| self.is_unused_specifier(ctx, specifier))
        })
    }

    /// Delete the entire import declaration, along with neighboring top-level declarations whose
    /// specifiers are all unused when their deletions would be adjacent. Every diagnostic in such
    /// a run reports the same fix, since the fixer would skip all but every other adjacent fix.
    fn delete_unused_import_declarations<'a>(
        &self,
        fixer: RuleFixer<'_, 'a>,
        ctx: &LintContext<'a>,
        import: &ImportDeclaration<'a>,
    ) -> RuleFix {
        let source = fixer.source_text();
        let mut span = Self::import_declaration_deletion_span(source, import.span);

        let body = &ctx.nodes().program().body;
        if let Ok(index) = body.binary_search_by_key(&import.span.start, |s| s.span().start) {
            for statement in body[..index].iter().rev() {
                let Statement::ImportDeclaration(prev) = statement else { break };
                let prev_span = Self::import_declaration_deletion_span(source, prev.span);
                if prev_span.end < span.start || !self.all_specifiers_unused(ctx, prev) {
                    break;
                }
                span.start = prev_span.start;
            }
            for statement in &body[index + 1..] {
                let Statement::ImportDeclaration(next) = statement else { break };
                let next_span = Self::import_declaration_deletion_span(source, next.span);
                if next_span.start > span.end || !self.all_specifiers_unused(ctx, next) {
                    break;
                }
                span.end = next_span.end;
            }
        }

        fixer.delete_range(span)
    }

    /// The span deleted for an import declaration, preserving blank lines appropriately
    #[expect(clippy::cast_possible_truncation)]
    fn import_declaration_deletion_span(source: &str, mut span: Span) -> Span {
        let before_import = &source[..(span.start as usize)];
        let leading_whitespace: u32 = before_import
            .chars()
            .rev()
            .take_while(|c| c.is_whitespace() && *c != '\n' && *c != '\r')
            .map(|c| c.len_utf8() as u32)
            .sum();
        span.start -= leading_whitespace;

        let after_import = &source[(span.end as usize)..];
        let trailing_newlines: u32 = after_import
            .chars()
            .take_while(|c| *c == '\n' || *c == '\r')
            .take(1)
            .map(|c| c.len_utf8() as u32)
            .sum();
        span.end += trailing_newlines;

        span
    }

    /// Remove the entire `{ ... }` block when all named imports are removed
    /// Transforms: `import Default, { Unused } from 'module'` -> `import Default from 'module'`
    fn remove_named_imports_block<'a>(
        fixer: RuleFixer<'_, 'a>,
        import: &ImportDeclaration<'a>,
        specifiers: &[ImportDeclarationSpecifier<'a>],
    ) -> RuleFix {
        let default_specifier = specifiers
            .iter()
            .find(|s| matches!(s, ImportDeclarationSpecifier::ImportDefaultSpecifier(_)));

        let Some(default_spec) = default_specifier else {
            debug_assert!(false, "Expected a default import when removing named imports block.");

            return fixer.noop();
        };

        let last_named = specifiers
            .iter()
            .rev()
            .find(|s| matches!(s, ImportDeclarationSpecifier::ImportSpecifier(_)));

        let Some(last_named) = last_named else {
            return fixer.noop();
        };

        let comma_offset = fixer
            .find_next_token_within(default_spec.span().end, last_named.span().start, ",")
            .expect("default and named import specifiers must be separated by a comma");
        let delete_start = default_spec.span().end + comma_offset;

        let brace_offset = fixer
            .find_next_token_within(last_named.span().end, import.span.end, "}")
            .expect("named import specifiers must be followed by a closing brace")
            + 1;
        let delete_end = last_named.span().end + brace_offset;

        let span = Span::new(delete_start, delete_end);
        fixer.delete_range(span)
    }

    /// Remove the default import when there are other imports remaining
    /// Transforms: `import Unused, { Used } from 'module'` -> `import { Used } from 'module'`
    /// Also handles: `import Unused, * as foo from 'module'` -> `import * as foo from 'module'`
    fn remove_default_import<'a>(
        fixer: RuleFixer<'_, 'a>,
        specifiers: &[ImportDeclarationSpecifier<'a>],
        specifier: &ImportDeclarationSpecifier<'a>,
    ) -> RuleFix {
        let default_span = specifier.span();

        let next = specifiers.get(1);

        let Some(next_spec) = next else {
            debug_assert!(
                false,
                "Expected another specifier after default import when removing default import."
            );
            return fixer.noop();
        };

        let next_specifier_start = match next_spec {
            ImportDeclarationSpecifier::ImportSpecifier(specifier) => {
                let next_token =
                    fixer.find_next_token_within(default_span.end, specifier.span.start, "{");

                let Some(next_token) = next_token else {
                    debug_assert!(
                        false,
                        "Could not find expected token after default import when removing default import."
                    );
                    return fixer.noop();
                };

                default_span.end + next_token
            }
            ImportDeclarationSpecifier::ImportNamespaceSpecifier(specifier) => specifier.span.start,
            ImportDeclarationSpecifier::ImportDefaultSpecifier(_) => {
                debug_assert!(
                    false,
                    "Expected named or namespace import after default import when removing default import."
                );
                return fixer.noop();
            }
        };

        let delete_span = Span::new(default_span.start, next_specifier_start);
        fixer.delete_range(delete_span)
    }

    /// Remove the namespace import when there's a default import remaining
    /// Transforms: `import a, * as Unused from 'module'` -> `import a from 'module'`
    fn remove_namespace_import<'a>(
        fixer: RuleFixer<'_, 'a>,
        specifiers: &[ImportDeclarationSpecifier<'a>],
        specifier: &ImportDeclarationSpecifier<'a>,
    ) -> RuleFix {
        let default_specifier = specifiers
            .iter()
            .find(|s| matches!(s, ImportDeclarationSpecifier::ImportDefaultSpecifier(_)));

        let Some(default_spec) = default_specifier else {
            debug_assert!(false, "Expected a default import when removing namespace import.");
            return fixer.noop();
        };

        // `import a, * as foo from 'module'` -> `import a from 'module'`
        let delete_start = default_spec.span().end;
        let delete_end = specifier.span().end;

        let span = Span::new(delete_start, delete_end);
        fixer.delete_range(span)
    }

    /// Remove a named specifier along with one adjacent separator. The separator after it is
    /// removed when a kept specifier follows, otherwise the one before it, so that removing
    /// several neighboring specifiers in one fix never produces overlapping ranges.
    #[expect(clippy::cast_possible_truncation)]
    fn remove_named_specifier<'a>(
        fixer: RuleFixer<'_, 'a>,
        named: &[(&ImportDeclarationSpecifier<'a>, bool)],
        named_pos: usize,
        followed_by_kept: bool,
    ) -> RuleFix {
        let source = fixer.source_text();
        let span = named[named_pos].0.span();
        let text_after = &source[(span.end as usize)..];

        if !followed_by_kept {
            // A kept specifier precedes this one because not every named specifier is removed.
            let Some((prev, _)) = named_pos.checked_sub(1).and_then(|i| named.get(i)) else {
                debug_assert!(false, "Expected a kept named import before the removed one.");
                return fixer.noop();
            };
            let mut delete_end = span.end;
            if named_pos == named.len() - 1 && text_after.trim_start().starts_with(',') {
                let trailing_space: u32 = text_after
                    .chars()
                    .take_while(|c| c.is_whitespace())
                    .map(|c| c.len_utf8() as u32)
                    .sum();
                delete_end += trailing_space;
            }
            return fixer.delete_range(Span::new(prev.span().end, delete_end));
        }

        let trailing = count_whitespace_or_commas(text_after.chars());
        if let Some((next_specifier, _)) = named.get(named_pos + 1)
            && let Some(offset) =
                fixer.find_next_token_within(span.end, next_specifier.span().start, ",")
            && offset >= trailing
        {
            // A comment stopped the whitespace scan before the separator.
            // Remove the comma separately to preserve the comment.
            let mut fix = fixer.new_fix_with_capacity(2);
            fix.push(fixer.delete_range(span));
            fix.push(fixer.delete_range(Span::sized(span.end + offset, 1)));
            return fix;
        }
        fixer.delete_range(span.expand_right(trailing))
    }
}
