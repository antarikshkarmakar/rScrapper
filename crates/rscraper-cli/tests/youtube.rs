use rscraper_cli::youtube::{
    extract_video_id, parse_caption_tracks, parse_json3_captions, parse_search_results,
    parse_xml_captions, select_caption_track,
};
use rscraper_core::Error;

#[test]
fn caption_tracks_are_extracted_from_balanced_player_json() {
    let tracks = parse_caption_tracks(include_str!("fixtures/youtube-player.html")).unwrap();

    assert_eq!(tracks.len(), 3);
    assert_eq!(tracks[0].language_code, "en");
    assert_eq!(tracks[0].name, "English \"Main\"");
    assert!(!tracks[0].is_generated);
    assert_eq!(
        tracks[0].base_url.as_str(),
        "https://www.youtube.com/api/timedtext?v=abc_def-123&lang=en&name=English%20%22Main%22"
    );
    assert_eq!(tracks[1].language_code, "en");
    assert!(tracks[1].is_generated);
    assert_eq!(tracks[2].language_code, "fr");
}

#[test]
fn caption_tracks_skip_json_key_and_assignment_decoys_before_real_player_data() {
    let html = format!(
        r#"<script>
          var jsonKeyDecoy = {{"ytInitialPlayerResponse": {{"notCaptions": true}}}};
          ytInitialPlayerResponse = {{"notCaptions": true}};
        </script>
        {}"#,
        include_str!("fixtures/youtube-player.html")
    );

    let tracks = parse_caption_tracks(&html).unwrap();

    assert_eq!(tracks.len(), 3);
    assert_eq!(tracks[0].name, "English \"Main\"");
}

#[test]
fn caption_tracks_ignore_comments_and_strings_before_token_spaced_assignment() {
    let html = format!(
        r#"{}
        {}
        {}
        {}
        {}"#,
        line_comment_player_decoy(),
        block_comment_player_decoy(),
        html_comment_player_decoy(),
        string_player_decoy(),
        token_spaced_player_assignment("en", "Real English"),
    );

    let tracks = parse_caption_tracks(&html).unwrap();

    assert_eq!(tracks.len(), 1);
    assert_eq!(tracks[0].language_code, "en");
    assert_eq!(tracks[0].name, "Real English");
}

#[test]
fn caption_tracks_ignore_line_comments_until_javascript_line_terminators() {
    for terminator in ["\n", "\r", "\u{2028}", "\u{2029}"] {
        let html = format!(
            "<script>// ytInitialPlayerResponse = {};{terminator}ytInitialPlayerResponse = {};</script>",
            compact_one_track_player_object("de", "Line Comment German"),
            one_track_player_object("en", "Real English"),
        );

        let tracks = parse_caption_tracks(&html).unwrap();

        assert_eq!(tracks.len(), 1, "terminator {terminator:?}");
        assert_eq!(tracks[0].language_code, "en", "terminator {terminator:?}");
        assert_eq!(tracks[0].name, "Real English", "terminator {terminator:?}");
    }
}

#[test]
fn caption_tracks_ignore_legacy_close_comment_before_real_assignment() {
    let html = format!(
        r#"<script>
          --> ytInitialPlayerResponse = {};
          ytInitialPlayerResponse = {};
        </script>"#,
        compact_one_track_player_object("de", "HTML Close German"),
        one_track_player_object("en", "Real English"),
    );

    let tracks = parse_caption_tracks(&html).unwrap();

    assert_eq!(tracks.len(), 1);
    assert_eq!(tracks[0].language_code, "en");
    assert_eq!(tracks[0].name, "Real English");
}

#[test]
fn caption_tracks_treat_html_open_comments_as_javascript_line_comments() {
    for terminator in ["\n", "\r", "\u{2028}", "\u{2029}"] {
        let html = format!(
            "<script><!-- ytInitialPlayerResponse = {};{terminator}ytInitialPlayerResponse = {};\n//--></script>",
            compact_one_track_player_object("de", "HTML Open German"),
            one_track_player_object("en", "Real English"),
        );

        let tracks = parse_caption_tracks(&html).unwrap();

        assert_eq!(tracks.len(), 1, "terminator {terminator:?}");
        assert_eq!(tracks[0].language_code, "en", "terminator {terminator:?}");
        assert_eq!(tracks[0].name, "Real English", "terminator {terminator:?}");
    }
}

#[test]
fn caption_tracks_ignore_template_literal_decoys_before_real_assignment() {
    let fake = one_track_player_object("de", "Template German");
    let real = one_track_player_object("en", "Real English");
    let html = format!(
        r#"<script>
          const ignored = `ytInitialPlayerResponse = {fake};
          ${{ ytInitialPlayerResponse = {fake}; }}
          escaped \` marker`;
          ytInitialPlayerResponse = {real};
        </script>"#
    );

    let tracks = parse_caption_tracks(&html).unwrap();

    assert_eq!(tracks.len(), 1);
    assert_eq!(tracks[0].language_code, "en");
    assert_eq!(tracks[0].name, "Real English");
}

#[test]
fn caption_tracks_ignore_arbitrarily_nested_template_literal_decoys() {
    let fake = one_track_player_object("de", "Nested Template German");
    let real = one_track_player_object("en", "Real English");
    let html = nested_template_decoy_script("ytInitialPlayerResponse", &fake, &real);

    let tracks = parse_caption_tracks(&html).unwrap();

    assert_eq!(tracks.len(), 1);
    assert_eq!(tracks[0].language_code, "en");
    assert_eq!(tracks[0].name, "Real English");
}

#[test]
fn caption_tracks_ignore_regex_literals_across_interpolation_token_contexts() {
    let fake = one_track_player_object("de", "Regex Template German");
    let real = one_track_player_object("en", "Real English");
    for (context, expression) in [
        ("expression start", r#"/[}`]/.test("value")"#),
        ("after operator", r#"false || /[}`]/.test("value")"#),
        (
            "after keyword",
            r#"(() => { return /[}`]/.test("value"); })()"#,
        ),
        ("after unary keyword", r#"typeof /[}`]/"#),
        ("after binary keyword", r#"value instanceof /[}`]/"#),
        (
            "regex beginning with equals",
            r#"/=/.test("=") && /[}`]/.test("value")"#,
        ),
        ("after opening delimiter", r#"(/[}`]/).test("value")"#),
        (
            "after delimiters",
            r#"([/[}`]/, { pattern: /[}`]/ }]).length"#,
        ),
    ] {
        let html = regex_template_decoy_script("ytInitialPlayerResponse", expression, &fake, &real);

        let tracks = parse_caption_tracks(&html).unwrap();

        assert_eq!(tracks.len(), 1, "context {context}");
        assert_eq!(tracks[0].language_code, "en", "context {context}");
        assert_eq!(tracks[0].name, "Real English", "context {context}");
    }
}

#[test]
fn caption_tracks_ignore_regex_escapes_classes_slashes_and_flags() {
    let fake = one_track_player_object("de", "Regex Character German");
    let real = one_track_player_object("en", "Real English");
    let html = regex_template_decoy_script(
        "ytInitialPlayerResponse",
        r#"/foo\/bar\`\}[\]}`/]/gim.test("value")"#,
        &fake,
        &real,
    );

    let tracks = parse_caption_tracks(&html).unwrap();

    assert_eq!(tracks.len(), 1);
    assert_eq!(tracks[0].language_code, "en");
    assert_eq!(tracks[0].name, "Real English");
}

#[test]
fn caption_tracks_preserve_regex_context_across_adjacent_comments() {
    let fake = one_track_player_object("de", "Regex Comment German");
    let real = one_track_player_object("en", "Real English");
    for (context, expression) in [
        ("leading block comment", r#"/* before *//[}`]/.test(value)"#),
        (
            "assignment comment",
            r#"value = /* before */ /[}`]/.test(value)"#,
        ),
        (
            "line comment after operator",
            "true ? // before regex\n/[}`]/.test(value) : false",
        ),
    ] {
        let html = regex_template_decoy_script("ytInitialPlayerResponse", expression, &fake, &real);

        let tracks = parse_caption_tracks(&html).unwrap();

        assert_eq!(tracks.len(), 1, "context {context}");
        assert_eq!(tracks[0].language_code, "en", "context {context}");
        assert_eq!(tracks[0].name, "Real English", "context {context}");
    }
}

#[test]
fn caption_tracks_do_not_confuse_division_or_division_assignment_with_regex() {
    let fake = one_track_player_object("de", "Division Template German");
    let real = one_track_player_object("en", "Real English");
    for expression in [
        "10 / 2",
        "value /= 2",
        "(10 + 2) / 3",
        "items[0] /* dividend */ / /* divisor */ 2",
        "value /* target */ /= /* value */ 2",
        "value++ / 2",
        r#""value" / 2"#,
        "`value` / 2",
        "/value/ / 2",
        "object.return / 2",
        "object.in / 2",
    ] {
        let html = regex_template_decoy_script("ytInitialPlayerResponse", expression, &fake, &real);

        let tracks = parse_caption_tracks(&html)
            .unwrap_or_else(|error| panic!("expression {expression}: {error:?}"));

        assert_eq!(tracks.len(), 1, "expression {expression}");
        assert_eq!(tracks[0].language_code, "en", "expression {expression}");
        assert_eq!(tracks[0].name, "Real English", "expression {expression}");
    }
}

#[test]
fn caption_tracks_ignore_regex_literals_inside_nested_templates() {
    let fake = one_track_player_object("de", "Nested Regex German");
    let real = one_track_player_object("en", "Real English");
    let html = nested_regex_template_decoy_script("ytInitialPlayerResponse", &fake, &real);

    let tracks = parse_caption_tracks(&html).unwrap();

    assert_eq!(tracks.len(), 1);
    assert_eq!(tracks[0].language_code, "en");
    assert_eq!(tracks[0].name, "Real English");
}

#[test]
fn caption_tracks_reject_unicode_identifier_decoys_before_real_assignment() {
    let fake = one_track_player_object("de", "Unicode German");
    let real = one_track_player_object("en", "Real English");
    for decoy in [
        format!("αytInitialPlayerResponse = {fake};"),
        format!("ytInitialPlayerResponseβ = {fake};"),
        format!(r#"\u{{03B1}}ytInitialPlayerResponse = {fake};"#),
        format!(r#"ytInitialPlayerResponse\u{{03B2}} = {fake};"#),
    ] {
        let html = format!("<script>{decoy}ytInitialPlayerResponse = {real};</script>");

        let tracks = parse_caption_tracks(&html).unwrap();

        assert_eq!(tracks.len(), 1, "decoy {decoy}");
        assert_eq!(tracks[0].language_code, "en", "decoy {decoy}");
        assert_eq!(tracks[0].name, "Real English", "decoy {decoy}");
    }
}

#[test]
fn caption_tracks_reject_object_property_colon_decoys_before_real_assignment() {
    let fake = one_track_player_object("de", "Object Property German");
    let real = one_track_player_object("en", "Real English");
    for decoy in [
        format!("const cfg = {{ ytInitialPlayerResponse: {fake} }};"),
        format!(r#"const cfg = {{ ["ytInitialPlayerResponse"]: {fake} }};"#),
    ] {
        let html = format!("<script>{decoy}ytInitialPlayerResponse = {real};</script>");

        let tracks = parse_caption_tracks(&html).unwrap();

        assert_eq!(tracks.len(), 1, "decoy {decoy}");
        assert_eq!(tracks[0].language_code, "en", "decoy {decoy}");
        assert_eq!(tracks[0].name, "Real English", "decoy {decoy}");
    }
}

#[test]
fn caption_tracks_accept_json_string_object_after_executable_assignment() {
    let object = one_track_player_object("en", "Quoted English");
    let quoted_object = serde_json::to_string(&object).unwrap();
    let html = format!("<script>ytInitialPlayerResponse = {quoted_object};</script>");

    let tracks = parse_caption_tracks(&html).unwrap();

    assert_eq!(tracks.len(), 1);
    assert_eq!(tracks[0].name, "Quoted English");
}

#[test]
fn caption_selection_prefers_requested_english_human_generated_then_first() {
    let tracks = parse_caption_tracks(include_str!("fixtures/youtube-player.html")).unwrap();

    assert_eq!(
        select_caption_track(&tracks, Some("fr")).unwrap().name,
        "Francais"
    );
    assert_eq!(
        select_caption_track(&tracks, Some("de")).unwrap().name,
        "English \"Main\""
    );
    assert_eq!(
        select_caption_track(&tracks[1..2], None).unwrap().name,
        "English auto"
    );
    assert_eq!(
        select_caption_track(&tracks[2..3], None).unwrap().name,
        "Francais"
    );
}

#[test]
fn unsafe_caption_urls_are_rejected_without_secret_leakage() {
    for unsafe_url in [
        "http://www.youtube.com/api/timedtext?v=abc_def-123",
        "https://www.youtube.com:444/api/timedtext?v=abc_def-123",
        "https://user:pass@www.youtube.com/api/timedtext?v=abc_def-123",
        "https://www.youtube.com.evil.example/api/timedtext?v=abc_def-123",
        "https://youtube.com.evil/api/timedtext?v=abc_def-123",
        "https://www.youtube.com/api/timedtext?v=abc_def-123&next=http%3A%2F%2Fwww.youtube.com%2Fwatch%3Fv%3Dabc_def-123",
        "https://www.youtube.com/api/timedtext?v=abc_def-123&next=https%3A%2F%2Fuser%3Apass%40www.youtube.com%2Fwatch%3Fv%3Dabc_def-123",
        "https://www.youtube.com/api/timedtext?v=abc_def-123&next=https%253A%252F%252Fevil.example%252Fsteal",
        "https://www.youtube.com/api/timedtext?v=abc_def-123&next=%20https%3A%2F%2Fevil.example%2Fsteal",
    ] {
        let html = player_with_caption_url(unsafe_url);
        let error = parse_caption_tracks(&html).unwrap_err();
        let diagnostic = format!("{error:?} {error}");
        assert!(matches!(error, Error::Policy(_)));
        assert!(!diagnostic.contains("user:pass"));
        assert!(!diagnostic.contains("secret-cookie-value"));
    }
}

#[test]
fn json3_captions_join_segments_decode_entities_and_preserve_gaps() {
    let text = parse_json3_captions(include_bytes!("fixtures/youtube-captions.json3")).unwrap();

    assert_eq!(text, "First line\n\nSecond & final line");
}

#[test]
fn xml_captions_join_text_decode_entities_and_preserve_gaps() {
    let text = parse_xml_captions(include_bytes!("fixtures/youtube-captions.xml")).unwrap();

    assert_eq!(text, "First line\n\nSecond & final line");
}

#[test]
fn video_ids_accept_the_documented_eleven_character_alphabet() {
    assert_eq!(
        extract_video_id("https://youtu.be/abc_def-123?t=4"),
        Some("abc_def-123".to_string())
    );
    assert_eq!(
        extract_video_id("abc_def-123"),
        Some("abc_def-123".to_string())
    );
    assert_eq!(extract_video_id("abc.def-123"), None);
    assert_eq!(extract_video_id("abc_def-12"), None);
}

#[test]
fn search_results_are_deserialized_from_embedded_json() {
    let results = parse_search_results(include_str!("fixtures/youtube-player.html"), 5).unwrap();

    assert_eq!(results.len(), 2);
    assert_eq!(results[0].title, "Escaped ☃ \"Title\"");
    assert_eq!(
        results[0].url,
        "https://www.youtube.com/watch?v=abc_def-123"
    );
    assert_eq!(results[1].title, "Runs Title");
}

#[test]
fn search_results_skip_json_key_and_assignment_decoys_before_real_initial_data() {
    let html = format!(
        r#"<script>
          var jsonKeyDecoy = {{"ytInitialData": {{"contents": []}}}};
          ytInitialData = {{"contents": []}};
        </script>
        {}"#,
        include_str!("fixtures/youtube-player.html")
    );

    let results = parse_search_results(&html, 5).unwrap();

    assert_eq!(results.len(), 2);
    assert_eq!(results[0].title, "Escaped ☃ \"Title\"");
}

#[test]
fn search_results_ignore_comments_and_strings_before_token_spaced_assignment() {
    let html = format!(
        r#"{}
        {}
        {}
        {}
        {}"#,
        line_comment_search_decoy(),
        block_comment_search_decoy(),
        html_comment_search_decoy(),
        string_search_decoy(),
        token_spaced_search_assignment("Real Search"),
    );

    let results = parse_search_results(&html, 5).unwrap();

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].title, "Real Search");
    assert_eq!(
        results[0].url,
        "https://www.youtube.com/watch?v=abc_def-123"
    );
}

#[test]
fn search_results_ignore_template_literal_decoys_before_real_assignment() {
    let fake = one_video_search_object("ZYXWVUT9876", "Template Search");
    let real = one_video_search_object("abc_def-123", "Real Search");
    let html = format!(
        r#"<script>
          const ignored = `ytInitialData = {fake};
          ${{ ytInitialData = {fake}; }}
          escaped \` marker`;
          ytInitialData = {real};
        </script>"#
    );

    let results = parse_search_results(&html, 5).unwrap();

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].title, "Real Search");
    assert_eq!(
        results[0].url,
        "https://www.youtube.com/watch?v=abc_def-123"
    );
}

#[test]
fn search_results_ignore_arbitrarily_nested_template_literal_decoys() {
    let fake = one_video_search_object("ZYXWVUT9876", "Nested Template Search");
    let real = one_video_search_object("abc_def-123", "Real Search");
    let html = nested_template_decoy_script("ytInitialData", &fake, &real);

    let results = parse_search_results(&html, 5).unwrap();

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].title, "Real Search");
    assert_eq!(
        results[0].url,
        "https://www.youtube.com/watch?v=abc_def-123"
    );
}

#[test]
fn search_results_ignore_regex_literals_inside_template_interpolation() {
    let fake = one_video_search_object("ZYXWVUT9876", "Regex Template Search");
    let real = one_video_search_object("abc_def-123", "Real Search");
    let html = regex_template_decoy_script("ytInitialData", r#"/[}`]/, 0"#, &fake, &real);

    let results = parse_search_results(&html, 5).unwrap();

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].title, "Real Search");
    assert_eq!(
        results[0].url,
        "https://www.youtube.com/watch?v=abc_def-123"
    );
}

#[test]
fn search_results_accept_state_after_legacy_html_open_wrapper() {
    let fake = one_video_search_object("ZYXWVUT9876", "HTML Open Search");
    let real = one_video_search_object("abc_def-123", "Real Search");
    let html =
        format!("<script><!-- ytInitialData = {fake};\nytInitialData = {real};\n//--></script>");

    let results = parse_search_results(&html, 5).unwrap();

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].title, "Real Search");
}

#[test]
fn search_results_reject_unicode_identifier_decoys_before_real_assignment() {
    let fake = one_video_search_object("ZYXWVUT9876", "Unicode Search");
    let real = one_video_search_object("abc_def-123", "Real Search");
    for decoy in [
        format!("αytInitialData = {fake};"),
        format!("ytInitialDataβ = {fake};"),
        format!(r#"\u{{03B1}}ytInitialData = {fake};"#),
        format!(r#"ytInitialData\u{{03B2}} = {fake};"#),
    ] {
        let html = format!("<script>{decoy}ytInitialData = {real};</script>");

        let results = parse_search_results(&html, 5).unwrap();

        assert_eq!(results.len(), 1, "decoy {decoy}");
        assert_eq!(results[0].title, "Real Search", "decoy {decoy}");
    }
}

#[test]
fn search_results_reject_object_property_colon_decoys_before_real_assignment() {
    let fake = one_video_search_object("ZYXWVUT9876", "Object Property Search");
    let real = one_video_search_object("abc_def-123", "Real Search");
    for decoy in [
        format!("const cfg = {{ ytInitialData: {fake} }};"),
        format!(r#"const cfg = {{ ["ytInitialData"]: {fake} }};"#),
    ] {
        let html = format!("<script>{decoy}ytInitialData = {real};</script>");

        let results = parse_search_results(&html, 5).unwrap();

        assert_eq!(results.len(), 1, "decoy {decoy}");
        assert_eq!(results[0].title, "Real Search", "decoy {decoy}");
    }
}

#[test]
fn consent_pages_report_layout_without_body_or_cookie_values() {
    let error = parse_caption_tracks(
        r#"<html><body>
          <form action="https://consent.youtube.com/save">secret-cookie-value</form>
        </body></html>"#,
    )
    .unwrap_err();
    let diagnostic = format!("{error:?} {error}");

    assert!(matches!(
        error,
        Error::UpstreamLayout { service: "youtube" }
    ));
    assert!(!diagnostic.contains("secret-cookie-value"));
    assert!(!diagnostic.contains("<html"));
}

fn player_with_caption_url(base_url: &str) -> String {
    format!(
        r#"<script>
        ytInitialPlayerResponse = {{
          "captions": {{
            "playerCaptionsTracklistRenderer": {{
              "captionTracks": [{{
                "baseUrl": "{base_url}",
                "languageCode": "en",
                "name": {{"simpleText": "English"}}
              }}]
            }}
          }}
        }};
        </script>"#
    )
}

fn token_spaced_player_assignment(language_code: &str, name: &str) -> String {
    format!(
        r#"<script>
        window[ "ytInitialPlayerResponse" ] /* comment after property */ =
          /* comment after assignment */ {};
        </script>"#,
        one_track_player_object(language_code, name)
    )
}

fn line_comment_player_decoy() -> String {
    format!(
        r#"<script>// ytInitialPlayerResponse = {};</script>"#,
        one_track_player_object("de", "Comment German")
    )
}

fn block_comment_player_decoy() -> String {
    format!(
        r#"<script>/* ytInitialPlayerResponse = {}; */</script>"#,
        one_track_player_object("de", "Block Comment German")
    )
}

fn html_comment_player_decoy() -> String {
    format!(
        r#"<script><!-- ytInitialPlayerResponse = {}; --></script>"#,
        one_track_player_object("de", "HTML Comment German")
    )
}

fn string_player_decoy() -> String {
    format!(
        r#"<script>const ignored = 'escaped \" // marker /* marker <!-- marker ytInitialPlayerResponse = {};';</script>"#,
        one_track_player_object("de", "String German")
    )
}

fn one_track_player_object(language_code: &str, name: &str) -> String {
    format!(
        r#"{{
          "captions": {{
            "playerCaptionsTracklistRenderer": {{
              "captionTracks": [{{
                "baseUrl": "https://www.youtube.com/api/timedtext?v=abc_def-123&lang={language_code}",
                "languageCode": "{language_code}",
                "name": {{"simpleText": "{name}"}}
              }}]
            }}
          }}
        }}"#
    )
}

fn compact_one_track_player_object(language_code: &str, name: &str) -> String {
    format!(
        r#"{{"captions":{{"playerCaptionsTracklistRenderer":{{"captionTracks":[{{"baseUrl":"https://www.youtube.com/api/timedtext?v=abc_def-123&lang={language_code}","languageCode":"{language_code}","name":{{"simpleText":"{name}"}}}}]}}}}}}"#
    )
}

fn token_spaced_search_assignment(title: &str) -> String {
    format!(
        r#"<script>
        window[ "ytInitialData" ] /* comment after property */ =
          /* comment after assignment */ {};
        </script>"#,
        one_video_search_object("abc_def-123", title)
    )
}

fn line_comment_search_decoy() -> String {
    format!(
        r#"<script>// ytInitialData = {};</script>"#,
        one_video_search_object("ZYXWVUT9876", "Comment Search")
    )
}

fn block_comment_search_decoy() -> String {
    format!(
        r#"<script>/* ytInitialData = {}; */</script>"#,
        one_video_search_object("ZYXWVUT9876", "Block Comment Search")
    )
}

fn html_comment_search_decoy() -> String {
    format!(
        r#"<script><!-- ytInitialData = {}; --></script>"#,
        one_video_search_object("ZYXWVUT9876", "HTML Comment Search")
    )
}

fn string_search_decoy() -> String {
    format!(
        r#"<script>const ignored = 'escaped \" // marker /* marker <!-- marker ytInitialData = {};';</script>"#,
        one_video_search_object("ZYXWVUT9876", "String Search")
    )
}

fn nested_template_decoy_script(name: &str, fake: &str, real: &str) -> String {
    [
        "<script>const ignored = `outer ${",
        r#"const quotedBrace = "}"; /* } */ // }"#,
        "\nconst nested = `",
        name,
        " = ",
        fake,
        "; ${",
        r#"const deeperBrace = "}";"#,
        "const deeper = `",
        name,
        " = ",
        fake,
        ";`; } nested tail`; } outer tail`;",
        name,
        " = ",
        real,
        ";</script>",
    ]
    .concat()
}

fn regex_template_decoy_script(name: &str, expression: &str, fake: &str, real: &str) -> String {
    [
        "<script>const ignored = `${",
        expression,
        "} raw ",
        name,
        " = ",
        fake,
        "; tail`;",
        name,
        " = ",
        real,
        ";</script>",
    ]
    .concat()
}

fn nested_regex_template_decoy_script(name: &str, fake: &str, real: &str) -> String {
    [
        "<script>const ignored = `outer ${/[}`]/, `nested ${/[}`]/} inner raw ",
        name,
        " = ",
        fake,
        ";`} outer raw ",
        name,
        " = ",
        fake,
        ";`;",
        name,
        " = ",
        real,
        ";</script>",
    ]
    .concat()
}

fn one_video_search_object(video_id: &str, title: &str) -> String {
    format!(
        r#"{{
          "contents": [{{
            "videoRenderer": {{
              "videoId": "{video_id}",
              "title": {{"simpleText": "{title}"}}
            }}
          }}]
        }}"#
    )
}
