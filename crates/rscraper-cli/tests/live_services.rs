//! Opt-in structural smoke tests. Exact parsing belongs to sanitized fixtures.

use rscraper_cli::{context::AppContext, github, social, web, youtube};
use rscraper_core::Error;
use url::Url;

fn require_live_tests() {
    assert_eq!(
        std::env::var("RSCRAPER_LIVE_TESTS").ok().as_deref(),
        Some("1"),
        "live tests require RSCRAPER_LIVE_TESTS=1"
    );
}

fn recognized_live_error(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<Error>()
        .is_some_and(recognized_core_error)
}

fn recognized_core_error(error: &Error) -> bool {
    matches!(
        error,
        Error::Authentication(_)
            | Error::RateLimited { .. }
            | Error::UpstreamLayout { .. }
            | Error::HttpStatus {
                status: 401 | 403 | 429,
                ..
            }
    )
}

#[tokio::test]
#[ignore = "live DuckDuckGo smoke; requires RSCRAPER_LIVE_TESTS=1"]
async fn live_duckduckgo_search_is_structural_or_reports_a_recognized_provider_state() {
    require_live_tests();
    let context = AppContext::try_default().unwrap();
    match web::search(&context, "rust programming language", 3, false).await {
        Ok(response) => {
            assert!(!response.results.is_empty());
            assert!(response
                .results
                .iter()
                .all(|hit| !hit.title.is_empty() && matches!(hit.url.scheme(), "http" | "https")));
            assert!(matches!(response.provider, "duckduckgo" | "bing"));
        }
        Err(error) => assert!(recognized_core_error(&error), "{error}"),
    }
}

#[tokio::test]
#[ignore = "live Bing fallback smoke; requires RSCRAPER_LIVE_TESTS=1"]
async fn live_bing_fallback_is_structural_or_reports_a_recognized_provider_state() {
    require_live_tests();
    let context = AppContext::try_default().unwrap();
    let endpoints = web::SearchEndpoints {
        duckduckgo: Url::parse("https://html.duckduckgo.com/__rscraper_force_fallback__").unwrap(),
        bing: Url::parse("https://www.bing.com/search").unwrap(),
    };
    match web::search_with_endpoints(&context, "rust programming language", 3, false, &endpoints)
        .await
    {
        Ok(response) => {
            assert_eq!(response.provider, "bing");
            assert!(response.fallback_warning.is_some());
            assert!(!response.results.is_empty());
        }
        Err(error) => assert!(recognized_core_error(&error), "{error}"),
    }
}

#[tokio::test]
#[ignore = "live YouTube search smoke; requires RSCRAPER_LIVE_TESTS=1"]
async fn live_youtube_search_is_non_empty_or_reports_layout_state() {
    require_live_tests();
    let context = AppContext::try_default().unwrap();
    if let Err(error) = youtube::search_with_context(&context, "rust language", 3, true).await {
        assert!(recognized_live_error(&error), "{error}");
    }
}

#[tokio::test]
#[ignore = "live YouTube captions smoke; requires RSCRAPER_LIVE_TESTS=1"]
async fn live_youtube_captions_are_non_empty_or_report_layout_state() {
    require_live_tests();
    let context = AppContext::try_default().unwrap();
    let video =
        std::env::var("RSCRAPER_LIVE_YOUTUBE_VIDEO").unwrap_or_else(|_| "dQw4w9WgXcQ".into());
    if let Err(error) = youtube::subs_with_context(&context, &video, true).await {
        assert!(recognized_live_error(&error), "{error}");
    }
}

#[tokio::test]
#[ignore = "live GitHub smoke; requires RSCRAPER_LIVE_TESTS=1"]
async fn live_github_repo_is_structural_or_reports_rate_limit() {
    require_live_tests();
    let context = AppContext::try_default().unwrap();
    match github::repo(&context, "rust-lang/rust").await {
        Ok(repo) => {
            assert_eq!(repo.name, "rust-lang/rust");
            assert!(repo.homepage.has_host());
        }
        Err(error) => assert!(recognized_core_error(&error), "{error}"),
    }
}

#[tokio::test]
#[ignore = "live Reddit smoke; requires RSCRAPER_LIVE_TESTS=1"]
async fn live_reddit_is_structural_or_reports_rate_limit() {
    require_live_tests();
    let context = AppContext::try_default().unwrap();
    match social::reddit(&context, "rust", 3).await {
        Ok(response) => {
            assert!(!response.results.is_empty());
            assert!(response
                .results
                .iter()
                .all(|post| !post.title.is_empty() && post.url.has_host()));
        }
        Err(error) => assert!(recognized_core_error(&error), "{error}"),
    }
}

#[tokio::test]
#[ignore = "live Bilibili smoke; requires RSCRAPER_LIVE_TESTS=1"]
async fn live_bilibili_is_structural_or_reports_rate_limit() {
    require_live_tests();
    let context = AppContext::try_default().unwrap();
    match social::bilibili(&context, "rust", 3).await {
        Ok(response) => {
            assert!(!response.results.is_empty());
            assert!(response
                .results
                .iter()
                .all(|video| !video.title.is_empty() && video.url.has_host()));
        }
        Err(error) => assert!(recognized_core_error(&error), "{error}"),
    }
}

#[tokio::test]
#[ignore = "live authenticated smoke; requires RSCRAPER_LIVE_TESTS=1 and configured cookies"]
async fn live_configured_authenticated_platforms_are_structural_or_recognized() {
    require_live_tests();
    let context = AppContext::try_default().unwrap();

    if context.config_dir.join("twitter.cookies.txt").exists() {
        match social::twitter(&context, Some("rust")).await {
            Ok(response) => assert!(response.count > 0),
            Err(error) => assert!(recognized_core_error(&error), "{error}"),
        }
    }
    if context.config_dir.join("xiaohongshu.cookies.txt").exists() {
        match social::xiaohongshu(&context, "rust").await {
            Ok(response) => assert!(response.count > 0),
            Err(error) => assert!(recognized_core_error(&error), "{error}"),
        }
    }
    if context.config_dir.join("linkedin.cookies.txt").exists() {
        match social::linkedin(&context, "rust").await {
            Ok(response) => assert!(response.count > 0),
            Err(error) => assert!(recognized_core_error(&error), "{error}"),
        }
    }
}
