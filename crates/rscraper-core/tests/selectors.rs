use std::collections::HashMap;
use std::panic;

use rscraper_core::{clean_text, Error, Fingerprint, Result as CoreResult, Sel, SelectorMemory};
use scraper::{ElementRef, Html};

fn assert_parse_error<T>(result: CoreResult<T>) {
    match result {
        Err(Error::Parse { kind, message }) => {
            assert!(!kind.is_empty(), "parse errors should name the parser");
            assert!(
                !message.is_empty(),
                "parse errors should explain the failure"
            );
        }
        Err(other) => panic!("expected Error::Parse, got {other:?}"),
        Ok(_) => panic!("expected Error::Parse, got Ok"),
    }
}

fn html(input: &str) -> Html {
    Html::parse_document(input)
}

fn ids(elements: &[ElementRef<'_>]) -> Vec<String> {
    elements
        .iter()
        .map(|element| {
            element
                .value()
                .attr("id")
                .expect("fixtures give every selected element an id")
                .to_string()
        })
        .collect()
}

fn attr<'a>(element: &'a ElementRef<'a>, name: &str) -> Option<&'a str> {
    element.value().attr(name)
}

#[test]
fn malformed_selector_inputs_return_parse_errors() {
    let document = html("<html><body><a href=\"/ok\">ok</a></body></html>");

    assert_parse_error(Sel::parse(""));
    assert_parse_error(Sel::parse("   "));
    assert_parse_error(Sel::parse("//a[@href='missing bracket'"));
    assert_parse_error(Sel::parse("//a[@href='unclosed]"));
    assert_parse_error(Sel::parse("//a[text()='Buy']"));
    assert_parse_error(Sel::parse("///li"));
    assert_parse_error(Sel::parse("//li[@href=]"));
    assert_parse_error(Sel::parse("//li[contains(@class)]"));

    assert_parse_error(Sel::Css("a[".to_string()).select(&document));
    assert_parse_error(Sel::Xpath("//a[@href='missing bracket'".to_string()).select(&document));
}

#[test]
fn xpath_child_axis_uses_direct_steps() {
    let document = html(
        r#"
        <html>
          <body id="body">
            <section><main id="nested-main"></main></section>
          </body>
        </html>
        "#,
    );

    let body = Sel::parse("/html/body").unwrap().select(&document).unwrap();
    assert_eq!(ids(&body), vec!["body"]);

    let nested_main = Sel::parse("/html/body/main")
        .unwrap()
        .select(&document)
        .unwrap();
    assert!(nested_main.is_empty());
}

#[test]
fn xpath_descendant_axis_preserves_document_order() {
    let document = html(
        r#"
        <html><body>
          <ol>
            <li id="first">First</li>
            <li id="second">Second</li>
          </ol>
          <section>
            <ul><li id="third">Third</li></ul>
          </section>
        </body></html>
        "#,
    );

    let items = Sel::parse("//li").unwrap().select(&document).unwrap();
    assert_eq!(ids(&items), vec!["first", "second", "third"]);
}

#[test]
fn xpath_position_is_one_based_after_filtering_and_deduplication() {
    let document = html(
        r#"
        <html><body>
          <div id="outer">
            <ul>
              <li id="first" class="target">First</li>
              <li id="second">Ignored</li>
              <li id="third" class="target">Third</li>
            </ul>
            <div id="nested">
              <ul><li id="nested-li" class="target">Nested</li></ul>
            </div>
          </div>
        </body></html>
        "#,
    );

    let second_filtered = Sel::parse("//li[contains(@class,'target')][2]")
        .unwrap()
        .select(&document)
        .unwrap();
    assert_eq!(ids(&second_filtered), vec!["third"]);

    let second_unique_from_overlapping_contexts = Sel::parse("//div//li[2]")
        .unwrap()
        .select(&document)
        .unwrap();
    assert_eq!(
        ids(&second_unique_from_overlapping_contexts),
        vec!["second"]
    );
}

#[test]
fn xpath_wildcard_child_axis_orders_nested_contexts_before_position() {
    let document = html(
        r#"
        <html><body>
          <div id="outer">
            <p id="p1">one</p>
            <div id="inner"><span id="span">nested</span></div>
            <p id="p2">two</p>
          </div>
        </body></html>
        "#,
    );

    let all_children = Sel::parse("//div/*").unwrap().select(&document).unwrap();
    assert_eq!(ids(&all_children), vec!["p1", "inner", "span", "p2"]);

    let fourth = Sel::parse("//div/*[4]").unwrap().select(&document).unwrap();
    assert_eq!(ids(&fourth), vec!["p2"]);
}

#[test]
fn xpath_attribute_predicates_support_existence_exact_and_contains() {
    let document = html(
        r#"
        <html><body>
          <a id="plain" href="/plain">Plain</a>
          <a id="primary" href="/buy" kind="primary" class="button card featured">Buy</a>
          <a id="secondary" kind="secondary" class="drop">Other</a>
        </body></html>
        "#,
    );

    let with_href = Sel::parse("//a[@href]").unwrap().select(&document).unwrap();
    assert_eq!(ids(&with_href), vec!["plain", "primary"]);

    let primary = Sel::parse("//a[@kind='primary']")
        .unwrap()
        .select(&document)
        .unwrap();
    assert_eq!(ids(&primary), vec!["primary"]);

    let card = Sel::parse("//a[contains(@class,'card')]")
        .unwrap()
        .select(&document)
        .unwrap();
    assert_eq!(ids(&card), vec!["primary"]);
}

#[test]
fn xpath_quoted_values_can_contain_separators_brackets_and_mixed_quotes() {
    let document = html(
        r#"
        <html><body>
          <a id="single-quoted" data-value="path/with,comma]and)paren">One</a>
          <a id="double-quoted" data-value="Bob's / route, still]ok)">Two</a>
          <a id="double-inside-single" data-value='She said "ship/it, now]please)"'>Three</a>
        </body></html>
        "#,
    );

    let single = Sel::parse("//a[@data-value='path/with,comma]and)paren']")
        .unwrap()
        .select(&document)
        .unwrap();
    assert_eq!(ids(&single), vec!["single-quoted"]);

    let double = Sel::parse("//a[@data-value=\"Bob's / route, still]ok)\"]")
        .unwrap()
        .select(&document)
        .unwrap();
    assert_eq!(ids(&double), vec!["double-quoted"]);

    let contains = Sel::parse("//a[contains(@data-value,'\"ship/it, now]please)\"')]")
        .unwrap()
        .select(&document)
        .unwrap();
    assert_eq!(ids(&contains), vec!["double-inside-single"]);
}

#[test]
fn xpath_overlapping_nested_contexts_do_not_duplicate_descendants() {
    let document = html(
        r#"
        <html><body>
          <div id="outer">
            <div id="inner">
              <ul>
                <li id="first">First</li>
                <li id="second">Second</li>
              </ul>
            </div>
          </div>
        </body></html>
        "#,
    );

    let items = Sel::parse("//div//li").unwrap().select(&document).unwrap();
    assert_eq!(ids(&items), vec!["first", "second"]);
}

#[test]
fn xpath_non_initial_descendant_axis_excludes_context_self_before_position() {
    let document = html(
        r#"
        <html><body>
          <div id="outer" class="ctx">
            <p id="p1"></p>
            <div id="inner" class="ctx">
              <span id="span"></span>
              <em id="em"></em>
            </div>
            <p id="p2"></p>
          </div>
          <section id="a" class="ctx"><span id="a-span"></span></section>
          <section id="b" class="ctx"><span id="b-span"></span></section>
        </body></html>
        "#,
    );

    let nested = Sel::parse("//div[contains(@class,'ctx')]//*[2]")
        .unwrap()
        .select(&document)
        .unwrap();
    assert_eq!(ids(&nested), vec!["inner"]);

    let strict_descendants = Sel::parse("//div[contains(@class,'ctx')]//*")
        .unwrap()
        .select(&document)
        .unwrap();
    assert_eq!(
        ids(&strict_descendants),
        vec!["p1", "inner", "span", "em", "p2"]
    );

    let mixed_axes = Sel::parse("//div[contains(@class,'ctx')]/*//em")
        .unwrap()
        .select(&document)
        .unwrap();
    assert_eq!(ids(&mixed_axes), vec!["em"]);

    let disjoint_position = Sel::parse("//section[contains(@class,'ctx')]//span[2]")
        .unwrap()
        .select(&document)
        .unwrap();
    assert_eq!(ids(&disjoint_position), vec!["b-span"]);
}

#[test]
fn empty_matches_are_successful_empty_results() {
    let document = html("<html><body><p>No articles</p></body></html>");
    let selector = Sel::parse("//article").unwrap();

    assert!(selector.select(&document).unwrap().is_empty());
    assert_eq!(selector.first_text(&document).unwrap(), None);

    let mut memory = SelectorMemory::new();
    assert_eq!(memory.remember("missing", &selector, &document), None);
    assert_eq!(memory.find("missing", &document), None);
}

#[test]
fn text_normalization_collapses_unicode_whitespace() {
    let document =
        html("<html><body><p id=\"price\">Price:\u{00a0}<span>€\u{2003}9</span></p></body></html>");

    assert_eq!(
        clean_text([" Price:", "\u{00a0}", "€\u{2003}9 "].into_iter()),
        "Price: € 9"
    );
    assert_eq!(
        Sel::parse("//p[@id='price']")
            .unwrap()
            .first_text(&document)
            .unwrap()
            .as_deref(),
        Some("Price: € 9")
    );
}

#[test]
fn selector_memory_refinds_by_normalized_text_class_tokens_and_stable_data_attrs() {
    let original = html(
        r#"
        <html><body>
          <button id="old-id" data-testid="checkout" data-action="purchase"
                  class="btn primary primary">Buy&nbsp; now</button>
        </body></html>
        "#,
    );
    let redesigned = html(
        r#"
        <html><body>
          <button id="new-id" data-testid="checkout" data-action="purchase"
                  class="primary refreshed btn">Buy	 now</button>
        </body></html>
        "#,
    );

    let mut memory = SelectorMemory::new();
    memory
        .remember("checkout", &Sel::parse("#old-id").unwrap(), &original)
        .unwrap();

    let found = memory.find("checkout", &redesigned).unwrap();
    assert_eq!(attr(&found, "id"), Some("new-id"));
}

#[test]
fn selector_memory_does_not_treat_class_substrings_as_token_matches() {
    let document =
        html("<html><body><p id=\"candidate\" class=\"reading\">Different</p></body></html>");
    let mut attrs = HashMap::new();
    attrs.insert("class".to_string(), "ad".to_string());
    let mut entries = HashMap::new();
    entries.insert(
        "promo".to_string(),
        Fingerprint {
            tag: "p".to_string(),
            text_snippet: "Advertisement".to_string(),
            attrs,
        },
    );
    let memory = SelectorMemory {
        entries,
        minimum_score: 0.45,
    };

    assert_eq!(memory.find("promo", &document), None);
}

#[test]
fn selector_memory_enforces_minimum_score_and_rejects_non_finite_thresholds() {
    let original =
        html("<html><body><span id=\"price\" data-testid=\"price\">$9.99</span></body></html>");
    let redesigned = html("<html><body><span id=\"new-price\">$9.99</span></body></html>");
    let mut memory = SelectorMemory::new();
    memory
        .remember("price", &Sel::parse("#price").unwrap(), &original)
        .unwrap();

    memory.minimum_score = 0.5;
    assert_eq!(
        attr(&memory.find("price", &redesigned).unwrap(), "id"),
        Some("new-price")
    );

    memory.minimum_score = 0.95;
    assert_eq!(memory.find("price", &redesigned), None);

    memory.minimum_score = f64::NAN;
    assert_eq!(memory.find("price", &redesigned), None);
}

#[test]
fn selector_memory_json_serialization_is_deterministic() {
    let document = html(
        r#"
        <html><body>
          <a id="cta" data-z="last" data-a="first" class="beta alpha">Buy now</a>
        </body></html>
        "#,
    );
    let mut memory = SelectorMemory::new();
    memory
        .remember("cta", &Sel::parse("#cta").unwrap(), &document)
        .unwrap();

    let first = memory.to_json().unwrap();
    let second = memory.to_json().unwrap();

    assert_eq!(first, second);
    assert_eq!(
        first,
        r#"{"entries":{"cta":{"tag":"a","text_snippet":"Buy now","attrs":{"class":"beta alpha","data-a":"first","data-z":"last","id":"cta"}}},"minimum_score":0.5}"#
    );
    assert!(SelectorMemory::from_json(&first)
        .unwrap()
        .entries
        .contains_key("cta"));
}

#[test]
fn selector_memory_to_json_rejects_non_finite_minimum_scores() {
    let entries: HashMap<String, Fingerprint> = [(
        "x".to_string(),
        Fingerprint {
            tag: "p".to_string(),
            text_snippet: "text".to_string(),
            attrs: HashMap::new(),
        },
    )]
    .into_iter()
    .collect();

    for minimum_score in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let memory = SelectorMemory {
            entries: entries.clone(),
            minimum_score,
        };
        assert!(
            memory.to_json().is_err(),
            "non-finite minimum_score {minimum_score:?} should not produce invalid JSON"
        );
    }
}

#[test]
fn large_hostile_xpath_input_returns_parse_error_without_panicking() {
    let hostile = format!(
        "//div[{}]",
        "contains(@data-x,'////]],)),,,')[".repeat(2_000)
    );
    let result = panic::catch_unwind(|| Sel::parse(&hostile));

    assert!(
        result.is_ok(),
        "parser should not panic on large malformed input"
    );
    assert_parse_error(result.unwrap());
}
