use super::{block::preferred_root_is_meaningful, MarkdownOptions};
use crate::Result;
use scraper::{ElementRef, Html};

const SKIPPED_ELEMENTS: &[&str] = &[
    "script", "style", "noscript", "template", "svg", "iframe", "canvas", "nav", "footer",
    "header", "aside", "form", "button", "select", "option", "input", "dialog", "head",
];

const EXCLUDED_CLASSES: &[&str] = &[
    "ad",
    "ads",
    "advert",
    "advertisement",
    "banner",
    "promo",
    "cookie",
    "newsletter",
    "sidebar",
    "related-posts",
    "share",
    "social-share",
];

#[derive(Clone, Copy)]
enum PreferredRoot {
    Main,
    RoleMain,
    Article,
}

pub(super) fn select_content_root<'a>(
    document: &'a Html,
    options: &MarkdownOptions,
) -> Result<ElementRef<'a>> {
    let body = document
        .select(&scraper::Selector::parse("body").expect("static selector"))
        .next()
        .unwrap_or_else(|| document.root_element());

    for preferred in [
        PreferredRoot::Main,
        PreferredRoot::RoleMain,
        PreferredRoot::Article,
    ] {
        if let Some(root) = first_meaningful_candidate(body, options, preferred)? {
            return Ok(root);
        }
    }
    Ok(body)
}

pub(super) fn is_excluded_element(element: &ElementRef<'_>) -> bool {
    SKIPPED_ELEMENTS.contains(&element.value().name())
        || element.value().attr("hidden").is_some()
        || element.value().attr("aria-hidden") == Some("true")
        || element.value().attr("class").is_some_and(|class| {
            class
                .split_ascii_whitespace()
                .any(|token| EXCLUDED_CLASSES.contains(&token))
        })
}

fn first_meaningful_candidate<'a>(
    root: ElementRef<'a>,
    options: &MarkdownOptions,
    preferred: PreferredRoot,
) -> Result<Option<ElementRef<'a>>> {
    let mut pending = vec![(root, false)];
    while let Some((element, inherited_excluded)) = pending.pop() {
        if let Some(sibling) = next_element_sibling(element) {
            pending.push((sibling, inherited_excluded));
        }

        let excluded = inherited_excluded || is_excluded_element(&element);
        if excluded {
            continue;
        }

        if matches_preference(element, preferred) {
            if preferred_root_is_meaningful(element, options)? {
                return Ok(Some(element));
            }
            // A candidate with no renderable content cannot contain a
            // meaningful candidate of the same or a lower priority.
            continue;
        }

        if let Some(child) = element.child_elements().next() {
            pending.push((child, excluded));
        }
    }
    Ok(None)
}

fn matches_preference(element: ElementRef<'_>, preferred: PreferredRoot) -> bool {
    match preferred {
        PreferredRoot::Main => element.value().name() == "main",
        PreferredRoot::RoleMain => element.value().attr("role") == Some("main"),
        PreferredRoot::Article => element.value().name() == "article",
    }
}

fn next_element_sibling<'a>(element: ElementRef<'a>) -> Option<ElementRef<'a>> {
    let mut sibling = element.next_sibling();
    while let Some(node) = sibling {
        if let Some(element) = ElementRef::wrap(node) {
            return Some(element);
        }
        sibling = node.next_sibling();
    }
    None
}
