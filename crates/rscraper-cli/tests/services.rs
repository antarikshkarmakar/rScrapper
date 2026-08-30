use rscraper_cli::context::AppContext;
use rscraper_cli::{github, output, social, web};
use rscraper_core::{Error, FetchClient, NetworkPolicy, OperationLimits};
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use url::Url;

const DDG: &str = include_str!("fixtures/ddg-results.html");
const BING: &str = include_str!("fixtures/bing-results.html");
const GITHUB_REPO: &[u8] = include_bytes!("fixtures/github-repo.json");
const GITHUB_README: &[u8] = include_bytes!("fixtures/github-readme.json");
const GITHUB_ISSUES_1: &[u8] = include_bytes!("fixtures/github-issues-page-1.json");
const GITHUB_ISSUES_2: &[u8] = include_bytes!("fixtures/github-issues-page-2.json");
const REDDIT: &[u8] = include_bytes!("fixtures/reddit-search.json");
const BILIBILI: &[u8] = include_bytes!("fixtures/bilibili-search.json");
const TWITTER: &str = include_str!("fixtures/twitter-articles.html");
const TWITTER_LOGIN: &str = include_str!("fixtures/twitter-login.html");
const XIAOHONGSHU: &str = include_str!("fixtures/xiaohongshu-state.html");
const LINKEDIN: &str = include_str!("fixtures/linkedin-people.html");
const LINKEDIN_CHECKPOINT: &str = include_str!("fixtures/linkedin-checkpoint.html");

#[test]
fn duckduckgo_results_are_scoped_to_each_container_and_redirects_use_url_parsing() {
    let hits = web::parse_duckduckgo_results(DDG, 20).unwrap();

    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].title, "Alpha & Beta");
    assert_eq!(hits[0].snippet, "First snippet.");
    assert_eq!(
        hits[0].url,
        Url::parse("https://example.com/alpha?a=1&b=2").unwrap()
    );
    assert_eq!(hits[1].snippet, "Second snippet.");
}

#[test]
fn bing_results_are_scoped_to_each_container() {
    let hits = web::parse_bing_results(BING, 20).unwrap();

    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].title, "Bing One");
    assert_eq!(hits[0].snippet, "One snippet.");
    assert_eq!(hits[1].snippet, "Two snippet.");
}

#[test]
fn credential_bearing_result_urls_are_rejected_before_output_or_debug() {
    const CREDENTIAL: &str = "credential-sentinel";
    let bing = format!(
        r#"<li class="b_algo"><h2><a href="https://user:{CREDENTIAL}@example.com/secret">Bad</a></h2><div class="b_caption"><p>bad</p></div></li><li class="b_algo"><h2><a href="https://example.com/safe">Safe</a></h2><div class="b_caption"><p>safe</p></div></li>"#
    );
    let hits = web::parse_bing_results(&bing, 5).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].title, "Safe");
    let search = web::SearchResponse {
        query: "fixture".into(),
        count: hits.len(),
        results: hits,
        provider: "bing",
        fallback_warning: None,
    };
    let search_output = format!(
        "{search:?} {} {}",
        serde_json::to_string(&search).unwrap(),
        output::render_search(&search)
    );
    assert!(!search_output.contains(CREDENTIAL));

    let reddit = format!(
        r#"{{"data":{{"children":[{{"data":{{"title":"Bad","permalink":"https://user:{CREDENTIAL}@evil.example/path","score":1,"author":"bad","selftext":"bad"}}}},{{"data":{{"title":"Safe","permalink":"/r/rust/comments/safe/","score":2,"author":"safe","selftext":"safe"}}}}]}}}}"#
    );
    let reddit = social::parse_reddit_response(reddit.as_bytes(), "rust", 5).unwrap();
    assert_eq!(reddit.results.len(), 1);
    let reddit_output = format!(
        "{reddit:?} {} {}",
        serde_json::to_string(&reddit).unwrap(),
        output::render_reddit(&reddit)
    );
    assert!(!reddit_output.contains(CREDENTIAL));

    let twitter = format!(
        r#"<article><div data-testid="tweetText">Bad</div><div data-testid="User-Name">Bad</div><a href="https://user:{CREDENTIAL}@x.com/bad/status/1">bad</a></article><article><div data-testid="tweetText">Safe</div><div data-testid="User-Name">Safe</div><a href="https://x.com/safe/status/2">safe</a></article>"#
    );
    let twitter = social::parse_twitter_response(&twitter, None).unwrap();
    assert_eq!(twitter.results.len(), 1);
    let twitter_output = format!("{twitter:?} {}", serde_json::to_string(&twitter).unwrap());
    assert!(!twitter_output.contains(CREDENTIAL));

    let repo = format!(
        r#"{{"full_name":"acme/widget","description":null,"stargazers_count":1,"forks_count":1,"language":null,"license":null,"open_issues_count":0,"html_url":"https://user:{CREDENTIAL}@github.com/acme/widget","default_branch":"main"}}"#
    );
    let error = github::parse_repo_response(repo.as_bytes()).unwrap_err();
    assert!(matches!(
        error,
        Error::Parse {
            kind: "github repo",
            ..
        }
    ));
    assert!(!format!("{error:?} {error}").contains(CREDENTIAL));
}

#[test]
fn missing_search_layout_is_an_explicit_error_but_confirmed_empty_is_not() {
    let error = web::parse_duckduckgo_results("<html><body>changed</body></html>", 5).unwrap_err();
    assert!(matches!(
        error,
        Error::UpstreamLayout {
            service: "duckduckgo"
        }
    ));

    let empty = web::parse_duckduckgo_results(
        "<html><body><div class=\"no-results\">No results found</div></body></html>",
        5,
    )
    .unwrap();
    assert!(empty.is_empty());
}

#[test]
fn reusable_search_validation_owns_query_and_result_count_bounds() {
    assert!(web::validate_search_input(&"x".repeat(1_024), 1).is_ok());
    assert!(web::validate_search_input("x", 20).is_ok());

    for (query, count) in [
        ("   ".to_owned(), 5),
        ("x".repeat(1_025), 5),
        ("x".to_owned(), 0),
        ("x".to_owned(), 21),
    ] {
        assert!(matches!(
            web::validate_search_input(&query, count),
            Err(Error::InvalidInput(_))
        ));
    }
}

#[test]
fn github_repo_and_readme_are_typed_and_base64_decoded() {
    let repo = github::parse_repo_response(GITHUB_REPO).unwrap();
    assert_eq!(repo.name, "acme/widget");
    assert_eq!(repo.stars, 42);
    assert_eq!(repo.license.as_deref(), Some("MIT"));
    assert_eq!(repo.homepage.as_str(), "https://github.com/acme/widget");

    let readme = github::parse_readme_response(GITHUB_README, "acme/widget").unwrap();
    assert_eq!(readme.repo, "acme/widget");
    assert_eq!(readme.readme, "# Fixture README\n\nHello.\n");
}

#[test]
fn github_owner_repo_requires_exactly_two_non_empty_segments() {
    assert_eq!(
        github::validate_owner_repo("acme/widget").unwrap(),
        ("acme", "widget")
    );
    for invalid in [
        "acme",
        "/widget",
        "acme/",
        "acme/widget/extra",
        "acme//widget",
    ] {
        assert!(matches!(
            github::validate_owner_repo(invalid),
            Err(Error::InvalidInput(_))
        ));
    }
}

#[test]
fn github_issue_pages_exclude_pull_requests_without_losing_later_issues() {
    let first = github::parse_issues_response(GITHUB_ISSUES_1).unwrap();
    let second = github::parse_issues_response(GITHUB_ISSUES_2).unwrap();

    assert_eq!(first.raw_count, 2);
    assert_eq!(first.issues.len(), 1);
    assert_eq!(first.issues[0].number, 11);
    assert_eq!(second.issues[0].number, 12);
}

#[test]
fn reddit_and_bilibili_use_typed_bounded_json_models() {
    let reddit = social::parse_reddit_response(REDDIT, "rust", 1).unwrap();
    assert_eq!(reddit.count, 1);
    assert_eq!(reddit.results[0].author, "ferris");
    assert_eq!(
        reddit.results[0].url.as_str(),
        "https://www.reddit.com/r/rust/comments/abc/fixture/"
    );

    let bilibili = social::parse_bilibili_response(BILIBILI, "rust", 1).unwrap();
    assert_eq!(bilibili.count, 1);
    assert_eq!(bilibili.results[0].title, "Learn Rust");
    assert_eq!(bilibili.results[0].author, "up-user");
}

#[test]
fn authenticated_platform_parsers_return_structured_results() {
    let twitter = social::parse_twitter_response(TWITTER, Some("fixture")).unwrap();
    assert_eq!(twitter.count, 2);
    assert_eq!(twitter.results[0].author, "Fixture User @fixture_user");
    assert_eq!(twitter.results[0].text, "First fixture tweet.");
    assert_eq!(
        twitter.results[0].url.as_str(),
        "https://x.com/fixture_user/status/100"
    );

    let xhs = social::parse_xiaohongshu_response(XIAOHONGSHU, "fixture").unwrap();
    assert_eq!(xhs.count, 2);
    assert_eq!(xhs.results[0].title, "Fixture note");
    assert_eq!(
        xhs.results[0].url.as_str(),
        "https://www.xiaohongshu.com/explore/note-1"
    );

    let linkedin = social::parse_linkedin_response(LINKEDIN, "fixture").unwrap();
    assert_eq!(linkedin.count, 2);
    assert_eq!(linkedin.results[0].name, "Fixture Person");
    assert_eq!(linkedin.results[0].headline, "Rust Engineer");
}

#[test]
fn authentication_and_layout_failures_are_distinct_and_body_safe() {
    let twitter =
        social::parse_twitter_response(TWITTER_LOGIN, Some("secret-cookie-value")).unwrap_err();
    assert!(matches!(twitter, Error::Authentication(_)));
    assert!(!twitter.to_string().contains("secret-cookie-value"));

    let linkedin =
        social::parse_linkedin_response(LINKEDIN_CHECKPOINT, "secret-cookie-value").unwrap_err();
    assert!(matches!(linkedin, Error::Authentication(_)));
    assert!(!linkedin.to_string().contains("secret-cookie-value"));

    let layout =
        social::parse_xiaohongshu_response("<html><body></body></html>", "fixture").unwrap_err();
    assert!(matches!(
        layout,
        Error::UpstreamLayout {
            service: "xiaohongshu"
        }
    ));
}

#[test]
fn authenticated_parsers_reject_result_urls_outside_the_platform() {
    let twitter = r#"<article><div data-testid="tweetText">text</div><a data-testid="User-Name">name</a><a href="https://evil.example/status/1">status</a></article>"#;
    assert!(matches!(
        social::parse_twitter_response(twitter, None),
        Err(Error::UpstreamLayout { service: "twitter" })
    ));

    let xiaohongshu = r#"<script id="__INITIAL_STATE__" type="application/json">{"search":{"notes":[{"id":"1","title":"note","author":"name","url":"https://evil.example/note/1"}]}}</script>"#;
    assert!(matches!(
        social::parse_xiaohongshu_response(xiaohongshu, "note"),
        Err(Error::UpstreamLayout {
            service: "xiaohongshu"
        })
    ));

    let linkedin = r#"<div class="reusable-search__result-container"><div class="entity-result__title-text"><a href="https://evil.example/in/name">Name</a></div></div>"#;
    assert!(matches!(
        social::parse_linkedin_response(linkedin, "name"),
        Err(Error::UpstreamLayout {
            service: "linkedin"
        })
    ));
}

#[test]
fn malformed_json_reports_a_typed_parse_error_without_echoing_the_body() {
    let secret = br#"{"secret":"secret-cookie-value""#;
    let error = social::parse_reddit_response(secret, "rust", 5).unwrap_err();
    assert!(matches!(error, Error::Parse { kind: "reddit", .. }));
    assert!(!error.to_string().contains("secret-cookie-value"));
}

#[test]
fn every_fixture_parser_reports_malformed_or_missing_layout_explicitly() {
    assert!(matches!(
        web::parse_bing_results("<html><body>changed</body></html>", 5),
        Err(Error::UpstreamLayout { service: "bing" })
    ));
    for error in [
        github::parse_repo_response(b"{").unwrap_err(),
        github::parse_readme_response(b"{", "acme/widget").unwrap_err(),
        github::parse_issues_response(b"{").unwrap_err(),
    ] {
        assert!(matches!(error, Error::Parse { .. }));
    }
    assert!(matches!(
        social::parse_bilibili_response(b"{", "rust", 5),
        Err(Error::Parse {
            kind: "bilibili",
            ..
        })
    ));
    assert!(matches!(
        social::parse_twitter_response("<html><body>changed</body></html>", None),
        Err(Error::UpstreamLayout { service: "twitter" })
    ));
    assert!(matches!(
        social::parse_xiaohongshu_response(
            "<script id=\"__INITIAL_STATE__\">{</script>",
            "fixture"
        ),
        Err(Error::Parse {
            kind: "xiaohongshu",
            ..
        })
    ));
    assert!(matches!(
        social::parse_linkedin_response("<html><body>changed</body></html>", "fixture"),
        Err(Error::UpstreamLayout {
            service: "linkedin"
        })
    ));
}

#[test]
fn reddit_and_bilibili_distinguish_empty_from_missing_or_unusable_lists() {
    let reddit_empty =
        social::parse_reddit_response(br#"{"data":{"children":[]}}"#, "rust", 5).unwrap();
    assert!(reddit_empty.results.is_empty());
    assert!(matches!(
        social::parse_reddit_response(br#"{"data":{}}"#, "rust", 5),
        Err(Error::UpstreamLayout { service: "reddit" })
    ));
    assert!(matches!(
        social::parse_reddit_response(
            br#"{"data":{"children":[{"data":{"title":"","permalink":"","score":0,"author":"","selftext":""}}]}}"#,
            "rust",
            5
        ),
        Err(Error::UpstreamLayout { service: "reddit" })
    ));

    let bilibili_empty =
        social::parse_bilibili_response(br#"{"code":0,"data":{"result":[]}}"#, "rust", 5).unwrap();
    assert!(bilibili_empty.results.is_empty());
    assert!(matches!(
        social::parse_bilibili_response(br#"{"code":0}"#, "rust", 5),
        Err(Error::UpstreamLayout {
            service: "bilibili"
        })
    ));
    assert!(matches!(
        social::parse_bilibili_response(
            br#"{"code":0,"data":{"result":[{"title":"bad","bvid":"","author":"","description":""}]}}"#,
            "rust",
            5
        ),
        Err(Error::UpstreamLayout { service: "bilibili" })
    ));
}

#[test]
fn bilibili_rejects_markup_only_titles_and_noncanonical_bvids_without_leakage() {
    let markup_only = social::parse_bilibili_response(
        br#"{"code":0,"data":{"result":[{"title":"<em class=\"keyword\"></em>","bvid":"BV1fixture01","author":"author","description":"description"}]}}"#,
        "rust",
        5,
    );
    assert!(matches!(
        markup_only,
        Err(Error::UpstreamLayout {
            service: "bilibili"
        })
    ));

    for bvid in ["BV1short", "av1fixture01", "BV1abc/../x", "BV1abc#part"] {
        let body = format!(
            r#"{{"code":0,"data":{{"result":[{{"title":"Valid title","bvid":"{bvid}","author":"author","description":"description"}}]}}}}"#
        );
        assert!(matches!(
            social::parse_bilibili_response(body.as_bytes(), "rust", 5),
            Err(Error::UpstreamLayout {
                service: "bilibili"
            })
        ));
    }

    const CREDENTIAL: &str = "credential-sentinel";
    let body = format!(
        r#"{{"code":0,"data":{{"result":[{{"title":"Valid title","bvid":"not-a-bvid?next=https://user:{CREDENTIAL}@evil.example/","author":"author","description":"description"}}]}}}}"#
    );
    let result = social::parse_bilibili_response(body.as_bytes(), "rust", 5);
    if let Ok(response) = &result {
        let exposed = format!(
            "{response:?} {} {}",
            serde_json::to_string(response).unwrap(),
            output::render_bilibili(response)
        );
        assert!(!exposed.contains(CREDENTIAL), "{exposed}");
    }
    assert!(matches!(
        result,
        Err(Error::UpstreamLayout {
            service: "bilibili"
        })
    ));
    if let Err(error) = result {
        assert!(!format!("{error:?} {error}").contains(CREDENTIAL));
    }

    let mixed = format!(
        r#"{{"code":0,"data":{{"result":[{{"title":"Invalid URL","bvid":"not-a-bvid?next=https://user:{CREDENTIAL}@evil.example/","author":"bad","description":"bad"}},{{"title":"Safe title","bvid":"BV1fixture01","author":"safe","description":"safe"}}]}}}}"#
    );
    let response = social::parse_bilibili_response(mixed.as_bytes(), "rust", 5).unwrap();
    assert_eq!(response.results.len(), 1);
    assert_eq!(
        response.results[0].url.as_str(),
        "https://www.bilibili.com/video/BV1fixture01"
    );
    let output = format!(
        "{response:?} {} {}",
        serde_json::to_string(&response).unwrap(),
        output::render_bilibili(&response)
    );
    assert!(!output.contains(CREDENTIAL));
}

#[tokio::test]
async fn authenticated_services_report_missing_cookie_files_as_authentication() {
    let directory = TempDir::new().unwrap();
    let context = AppContext {
        fetch: FetchClient::builder()
            .policy(NetworkPolicy::AllowPrivate)
            .build()
            .unwrap(),
        browser: None,
        config_dir: directory.path().to_path_buf(),
    };

    assert!(matches!(
        social::twitter(&context, None).await,
        Err(Error::Authentication(_))
    ));
    assert!(matches!(
        social::xiaohongshu(&context, "rust").await,
        Err(Error::Authentication(_))
    ));
    assert!(matches!(
        social::linkedin(&context, "rust").await,
        Err(Error::Authentication(_))
    ));
}

#[tokio::test]
async fn search_falls_back_on_primary_error_and_keeps_a_sanitized_warning() {
    let server = TestServer::spawn(|target, _| {
        if target.starts_with("/ddg") {
            TestResponse::html(503, "primary-response-secret")
        } else if target.starts_with("/bing") {
            TestResponse::html(200, BING)
        } else {
            TestResponse::html(404, "missing")
        }
    })
    .await;
    let context = local_context();
    let endpoints = web::SearchEndpoints {
        duckduckgo: server.url("/ddg"),
        bing: server.url("/bing"),
    };

    let response = web::search_with_endpoints(&context, "rust", 2, false, &endpoints)
        .await
        .unwrap();

    assert_eq!(response.provider, "bing");
    assert_eq!(response.count, 2);
    let warning = response.fallback_warning.unwrap();
    assert!(warning.contains("DuckDuckGo"));
    assert!(!warning.contains("primary-response-secret"));
    assert_eq!(server.max_in_flight(), 1);
}

#[tokio::test]
async fn optional_scraping_is_bounded_to_four_preserves_order_and_keeps_per_hit_errors() {
    let server = TestServer::spawn(|target, address| {
        if target.starts_with("/ddg") {
            let mut html = String::from("<html><body>");
            for index in 0..7 {
                html.push_str(&format!(
                    "<div class=\"result\"><a class=\"result__a\" href=\"http://{address}/page/{index}\">Hit {index}</a><div class=\"result__snippet\">Snippet {index}</div></div>"
                ));
            }
            html.push_str("</body></html>");
            TestResponse::html(200, html)
        } else if target.starts_with("/page/") {
            let index = target
                .trim_start_matches("/page/")
                .split('?')
                .next()
                .unwrap()
                .parse::<usize>()
                .unwrap();
            if index == 4 {
                TestResponse::html(500, "page-response-secret").delayed()
            } else {
                TestResponse::html(200, format!("<main><p>Page {index}</p></main>")).delayed()
            }
        } else {
            TestResponse::html(404, "missing")
        }
    })
    .await;
    let context = local_context();
    let endpoints = web::SearchEndpoints {
        duckduckgo: server.url("/ddg"),
        bing: server.url("/bing"),
    };

    let response = web::search_with_endpoints(&context, "rust", 7, true, &endpoints)
        .await
        .unwrap();

    assert_eq!(response.count, 7);
    for (index, hit) in response.results.iter().enumerate() {
        assert_eq!(hit.title, format!("Hit {index}"));
        if index == 4 {
            assert!(hit.markdown.is_none());
            let error = hit.scrape_error.as_deref().unwrap();
            assert!(error.contains("HTTP status"));
            assert!(!error.contains("page-response-secret"));
        } else {
            assert_eq!(
                hit.markdown.as_deref(),
                Some(format!("Page {index}").as_str())
            );
            assert!(hit.scrape_error.is_none());
        }
    }
    assert_eq!(server.max_in_flight(), 4);
}

#[tokio::test]
async fn optional_scraping_shares_the_markdown_budget_across_results() {
    let server = TestServer::spawn(|target, address| {
        if target.starts_with("/ddg") {
            let mut html = String::from("<html><body>");
            for index in 0..3 {
                html.push_str(&format!(
                    "<div class=\"result\"><a class=\"result__a\" href=\"http://{address}/page/{index}\">Hit {index}</a><div class=\"result__snippet\">Snippet {index}</div></div>"
                ));
            }
            html.push_str("</body></html>");
            TestResponse::html(200, html)
        } else if target.starts_with("/page/") {
            TestResponse::html(200, "<main><p>12345678901234567890</p></main>")
        } else {
            TestResponse::html(404, "missing")
        }
    })
    .await;
    let context = local_context_with_output_limit(48);
    let endpoints = web::SearchEndpoints {
        duckduckgo: server.url("/ddg"),
        bing: server.url("/bing"),
    };

    let response = web::search_with_endpoints(&context, "rust", 3, true, &endpoints)
        .await
        .unwrap();
    let markdown_chars = response
        .results
        .iter()
        .filter_map(|hit| hit.markdown.as_deref())
        .map(str::chars)
        .map(Iterator::count)
        .sum::<usize>();

    assert!(
        markdown_chars <= 48,
        "used {markdown_chars} Markdown characters"
    );
}

#[tokio::test]
async fn github_service_checks_status_rate_limits_and_paginates_non_pr_issues() {
    let server = TestServer::spawn(|target, _| {
        if target.starts_with("/api/repos/acme/widget/issues") && target.contains("page=1") {
            TestResponse::json(200, GITHUB_ISSUES_1)
        } else if target.starts_with("/api/repos/acme/widget/issues") && target.contains("page=2") {
            TestResponse::json(200, GITHUB_ISSUES_2)
        } else if target == "/api/repos/acme/widget/readme" {
            TestResponse::json(200, GITHUB_README)
        } else if target == "/api/repos/acme/widget" {
            TestResponse::json(200, GITHUB_REPO)
        } else if target == "/api/repos/limited/repo" {
            TestResponse::json(429, include_bytes!("fixtures/github-rate-limit.json"))
                .header("Retry-After", "23")
                .header("X-RateLimit-Remaining", "0")
        } else if target == "/api/repos/missing/repo" {
            TestResponse::html(404, "not-json-response-secret")
        } else {
            TestResponse::html(404, "missing")
        }
    })
    .await;
    let context = local_context();
    let api_base = server.url("/api/");

    let repo = github::repo_with_api_base(&context, "acme/widget", &api_base)
        .await
        .unwrap();
    assert_eq!(repo.name, "acme/widget");
    let readme = github::readme_with_api_base(&context, "acme/widget", &api_base)
        .await
        .unwrap();
    assert!(readme.readme.starts_with("# Fixture README"));
    let issues = github::issues_with_api_base(&context, "acme/widget", 2, &api_base)
        .await
        .unwrap();
    assert_eq!(
        issues
            .issues
            .iter()
            .map(|issue| issue.number)
            .collect::<Vec<_>>(),
        vec![11, 12]
    );

    let limited = github::repo_with_api_base(&context, "limited/repo", &api_base)
        .await
        .unwrap_err();
    assert!(matches!(
        limited,
        Error::RateLimited {
            retry_after_secs: Some(23)
        }
    ));

    let missing = github::repo_with_api_base(&context, "missing/repo", &api_base)
        .await
        .unwrap_err();
    assert!(matches!(missing, Error::HttpStatus { status: 404, .. }));
    assert!(!missing.to_string().contains("not-json-response-secret"));
}

#[tokio::test]
async fn github_issue_pagination_ceiling_is_an_explicit_error_not_partial_success() {
    let server = TestServer::spawn(|target, _| {
        if target.starts_with("/api/repos/acme/widget/issues") {
            TestResponse::json(
                200,
                br#"[{"number":1,"title":"PR only","html_url":"https://github.com/acme/widget/pull/1","pull_request":{}}]"#,
            )
        } else {
            TestResponse::html(404, "missing")
        }
    })
    .await;
    let context = local_context();

    let error = github::issues_with_api_base(&context, "acme/widget", 1, &server.url("/api/"))
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        Error::Parse {
            kind: "github issues",
            ..
        }
    ));
}

fn local_context() -> AppContext {
    AppContext {
        fetch: FetchClient::builder()
            .policy(NetworkPolicy::AllowPrivate)
            .build()
            .unwrap(),
        browser: None,
        config_dir: std::path::PathBuf::new(),
    }
}

fn local_context_with_output_limit(max_output_chars: usize) -> AppContext {
    let limits = OperationLimits {
        max_output_chars,
        ..OperationLimits::default()
    };
    AppContext {
        fetch: FetchClient::builder()
            .policy(NetworkPolicy::AllowPrivate)
            .limits(limits)
            .build()
            .unwrap(),
        browser: None,
        config_dir: std::path::PathBuf::new(),
    }
}

#[derive(Clone)]
struct TestResponse {
    status: u16,
    content_type: &'static str,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    delay: bool,
}

impl TestResponse {
    fn html(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            content_type: "text/html; charset=utf-8",
            headers: Vec::new(),
            body: body.into(),
            delay: false,
        }
    }

    fn json(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            content_type: "application/json",
            headers: Vec::new(),
            body: body.into(),
            delay: false,
        }
    }

    fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }

    fn delayed(mut self) -> Self {
        self.delay = true;
        self
    }
}

type Handler = Arc<dyn Fn(&str, SocketAddr) -> TestResponse + Send + Sync>;

struct TestServer {
    address: SocketAddr,
    max_in_flight: Arc<AtomicUsize>,
    _task: tokio::task::JoinHandle<()>,
}

impl TestServer {
    async fn spawn<F>(handler: F) -> Self
    where
        F: Fn(&str, SocketAddr) -> TestResponse + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let handler: Handler = Arc::new(handler);
        let in_flight = Arc::new(AtomicUsize::new(0));
        let max_in_flight = Arc::new(AtomicUsize::new(0));
        let task_in_flight = Arc::clone(&in_flight);
        let task_max = Arc::clone(&max_in_flight);
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let handler = Arc::clone(&handler);
                let in_flight = Arc::clone(&task_in_flight);
                let max_in_flight = Arc::clone(&task_max);
                tokio::spawn(async move {
                    handle_connection(stream, address, handler, in_flight, max_in_flight).await;
                });
            }
        });
        Self {
            address,
            max_in_flight,
            _task: task,
        }
    }

    fn url(&self, path: &str) -> Url {
        Url::parse(&format!("http://{}{}", self.address, path)).unwrap()
    }

    fn max_in_flight(&self) -> usize {
        self.max_in_flight.load(Ordering::SeqCst)
    }
}

fn handle_connection(
    mut stream: tokio::net::TcpStream,
    address: SocketAddr,
    handler: Handler,
    in_flight: Arc<AtomicUsize>,
    max_in_flight: Arc<AtomicUsize>,
) -> Pin<Box<dyn Future<Output = ()> + Send>> {
    Box::pin(async move {
        let mut request = Vec::new();
        let mut buffer = [0_u8; 1024];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let Ok(read) = stream.read(&mut buffer).await else {
                return;
            };
            if read == 0 {
                return;
            }
            request.extend_from_slice(&buffer[..read]);
            if request.len() > 16 * 1024 {
                return;
            }
        }
        let request = String::from_utf8_lossy(&request);
        let target = request
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or("/");
        let response = handler(target, address);
        let active = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        max_in_flight.fetch_max(active, Ordering::SeqCst);
        if response.delay {
            tokio::time::sleep(Duration::from_millis(80)).await;
        }
        let reason = match response.status {
            200 => "OK",
            429 => "Too Many Requests",
            503 => "Service Unavailable",
            _ => "Not Found",
        };
        let mut head = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n",
            response.status,
            reason,
            response.content_type,
            response.body.len()
        );
        for (name, value) in response.headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str("\r\n");
        let _ = stream.write_all(head.as_bytes()).await;
        let _ = stream.write_all(&response.body).await;
        let _ = stream.shutdown().await;
        in_flight.fetch_sub(1, Ordering::SeqCst);
    })
}
