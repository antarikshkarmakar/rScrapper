#[path = "crawler/support.rs"]
mod fixture;

use fixture::{ControlledServer, FixtureAction, FixtureResponse};
use futures_util::StreamExt;
use rscraper_core::robots::RobotsPolicy;
use rscraper_core::spider::{
    CrawlConfig, CrawlResult, CrawlStats, CrawlStatsSnapshot, Crawler, CRAWLER_USER_AGENT,
    MAX_LINKS_PER_PAGE,
};
use rscraper_core::urlnorm::{
    is_destructive_url, normalize_url, resolve_and_normalize, same_origin, within_origin_scope,
};
use rscraper_core::{Error, FetchClient, NetworkPolicy, OperationLimits};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;
use url::Url;

fn test_client() -> FetchClient {
    test_client_with_timeout(Duration::from_secs(2))
}

fn test_client_with_timeout(request_timeout: Duration) -> FetchClient {
    FetchClient::builder()
        .policy(NetworkPolicy::AllowPrivate)
        .limits(OperationLimits {
            connect_timeout: Duration::from_secs(1),
            request_timeout,
            ..OperationLimits::default()
        })
        .build()
        .unwrap()
}

fn crawl_config(start_url: Url) -> CrawlConfig {
    CrawlConfig {
        start_url,
        max_pages: 20,
        concurrency: 4,
        same_origin_only: true,
        include_subdomains: false,
        respect_robots: false,
        minimum_delay: Duration::ZERO,
        proxies: Vec::new(),
    }
}

async fn collect(
    crawler: &Crawler,
    config: CrawlConfig,
) -> (
    Vec<rscraper_core::Result<rscraper_core::spider::CrawlResult>>,
    Arc<rscraper_core::spider::CrawlStats>,
) {
    let (stream, _control, stats) = crawler.stream(config).unwrap();
    let output = tokio::time::timeout(Duration::from_secs(3), stream.collect::<Vec<_>>())
        .await
        .expect("crawler did not terminate");
    (output, stats)
}

fn assert_monotonic(before: CrawlStatsSnapshot, after: CrawlStatsSnapshot) {
    assert!(after.attempted >= before.attempted);
    assert!(after.succeeded >= before.succeeded);
    assert!(after.failed >= before.failed);
    assert!(after.queued >= before.queued);
    assert!(after.skipped >= before.skipped);
}

#[tokio::test]
async fn task7_public_debug_diagnostics_redact_crawl_data_and_urls() {
    let config = CrawlConfig {
        start_url: Url::parse(
            "https://config-user-sentinel:config-password-sentinel@config-host-sentinel.example/config-path-sentinel?config-query-sentinel#config-fragment-sentinel",
        )
        .unwrap(),
        max_pages: 19,
        concurrency: 7,
        same_origin_only: false,
        include_subdomains: true,
        respect_robots: true,
        minimum_delay: Duration::from_millis(413),
        proxies: vec![Url::parse(
            "http://proxy-user-sentinel:proxy-password-sentinel@proxy-host-sentinel.example:8080/proxy-path-sentinel?proxy-query-sentinel#proxy-fragment-sentinel",
        )
        .unwrap()],
    };
    let result = CrawlResult {
        url: Url::parse(
            "https://result-user-sentinel:result-password-sentinel@result-host-sentinel.example/result-path-sentinel?result-query-sentinel#result-fragment-sentinel",
        )
        .unwrap(),
        status: 207,
        html: "result-html-secret-sentinel".into(),
        links: vec![Url::parse(
            "https://link-user-sentinel:link-password-sentinel@link-host-sentinel.example/link-path-sentinel?link-query-sentinel#link-fragment-sentinel",
        )
        .unwrap()],
    };
    let robots_policy = RobotsPolicy::parse(
        "User-agent: rscraper\nDisallow: /robots-rule-body-secret-sentinel\nCrawl-delay: 0.371\n",
        "rscraper",
    );

    let server = ControlledServer::spawn(|_| {
        FixtureAction::Respond(FixtureResponse::html("<p>diagnostic fixture</p>"))
    })
    .await;
    let crawler = Crawler::new(test_client());
    let (stream, control, stats) = crawler.stream(crawl_config(server.url("/"))).unwrap();
    let diagnostics = [
        ("crawl config", format!("{config:?}")),
        ("crawl result", format!("{result:?}")),
        ("crawl control", format!("{control:?}")),
        ("crawl stats snapshot", format!("{:?}", stats.snapshot())),
        ("crawl stats", format!("{stats:?}")),
        ("empty crawl stats", format!("{:?}", CrawlStats::default())),
        ("robots policy", format!("{robots_policy:?}")),
    ];

    let sentinels = [
        "config-user-sentinel",
        "config-password-sentinel",
        "config-host-sentinel",
        "config-path-sentinel",
        "config-query-sentinel",
        "config-fragment-sentinel",
        "proxy-user-sentinel",
        "proxy-password-sentinel",
        "proxy-host-sentinel",
        "proxy-path-sentinel",
        "proxy-query-sentinel",
        "proxy-fragment-sentinel",
        "result-user-sentinel",
        "result-password-sentinel",
        "result-host-sentinel",
        "result-path-sentinel",
        "result-query-sentinel",
        "result-fragment-sentinel",
        "result-html-secret-sentinel",
        "link-user-sentinel",
        "link-password-sentinel",
        "link-host-sentinel",
        "link-path-sentinel",
        "link-query-sentinel",
        "link-fragment-sentinel",
        "robots-rule-body-secret-sentinel",
    ];
    let mut leaks = Vec::new();
    for (kind, diagnostic) in &diagnostics {
        let diagnostic = diagnostic.to_ascii_lowercase();
        for sentinel in sentinels {
            if diagnostic.contains(sentinel) {
                leaks.push(format!("{kind} leaked {sentinel}: {diagnostic}"));
            }
        }
    }

    control.cancel();
    tokio::time::timeout(Duration::from_secs(1), stream.collect::<Vec<_>>())
        .await
        .expect("cancelled diagnostic crawl did not terminate");

    assert!(leaks.is_empty(), "{}", leaks.join("\n"));
    assert!(diagnostics[0].1.contains("max_pages: 19"));
    assert!(diagnostics[0].1.contains("concurrency: 7"));
    assert!(diagnostics[0].1.contains("same_origin_only: false"));
    assert!(diagnostics[0].1.contains("include_subdomains: true"));
    assert!(diagnostics[0].1.contains("respect_robots: true"));
    assert!(diagnostics[0].1.contains("413ms"));
    assert!(diagnostics[0].1.contains("proxy_count: 1"));
    assert!(diagnostics[1].1.contains("status: 207"));
    assert!(diagnostics[1].1.contains("html_len: 27"));
    assert!(diagnostics[1].1.contains("link_count: 1"));
    assert!(diagnostics[2].1.contains("CrawlControl"));
    assert!(diagnostics[3].1.contains("CrawlStatsSnapshot"));
    assert!(diagnostics[4].1.contains("CrawlStats"));
    assert!(diagnostics[5].1.contains("CrawlStats"));
    assert!(diagnostics[6].1.contains("rule_count: 1"));
    assert!(diagnostics[6].1.contains("crawl_delay_present: true"));
}

#[test]
fn canonicalization_preserves_meaning_and_defines_identity() {
    let normalized = normalize_url(
        &Url::parse("HTTP://EXAMPLE.COM:80/a/../ordered?b=2&a=1&b=3#section").unwrap(),
    )
    .unwrap();
    assert_eq!(
        normalized.as_str(),
        "http://example.com/ordered?b=2&a=1&b=3"
    );

    let base = Url::parse("http://example.com/blog/post").unwrap();
    assert_eq!(
        resolve_and_normalize(&base, "child#one").unwrap(),
        Url::parse("http://example.com/blog/child").unwrap()
    );
    assert_eq!(
        resolve_and_normalize(&base, "child#two").unwrap(),
        Url::parse("http://example.com/blog/child").unwrap()
    );
}

#[test]
fn canonicalization_rejects_unsupported_schemes_and_credentials() {
    for candidate in [
        "mailto:person@example.com",
        "javascript:alert(1)",
        "data:text/plain,nope",
        "ftp://example.com/file",
        "http://user@example.com/",
        "https://user:secret@example.com/",
    ] {
        assert!(
            normalize_url(&Url::parse(candidate).unwrap()).is_err(),
            "accepted {candidate}"
        );
    }
}

#[test]
fn origin_scope_keeps_scheme_port_and_host_label_boundaries() {
    let start = Url::parse("https://example.com/root").unwrap();
    let same = Url::parse("https://EXAMPLE.com:443/other").unwrap();
    let scheme = Url::parse("http://example.com/other").unwrap();
    let port = Url::parse("https://example.com:444/other").unwrap();
    let child = Url::parse("https://docs.example.com/other").unwrap();
    let suffix_attack = Url::parse("https://notexample.com/other").unwrap();

    assert!(same_origin(&start, &same));
    assert!(!same_origin(&start, &scheme));
    assert!(!same_origin(&start, &port));
    assert!(within_origin_scope(&start, &same, false));
    assert!(!within_origin_scope(&start, &child, false));
    assert!(within_origin_scope(&start, &child, true));
    assert!(!within_origin_scope(&start, &suffix_attack, true));
    assert!(!within_origin_scope(
        &start,
        &Url::parse("http://docs.example.com/other").unwrap(),
        true
    ));
    assert!(!within_origin_scope(
        &start,
        &Url::parse("https://docs.example.com:444/other").unwrap(),
        true
    ));
}

#[test]
fn destructive_navigation_uses_token_boundaries() {
    for blocked in [
        "https://example.com/account/logout",
        "https://example.com/account/sign-out",
        "https://example.com/posts/delete/7",
        "https://example.com/remove-item",
        "https://example.com/account?ACTION=delete",
        "https://example.com/account?do=signout",
        "https://example.com/account?remove=true",
        "https://example.com/account?action=deactivate-user",
        "https://example.com/%6Cogout",
    ] {
        assert!(
            is_destructive_url(&Url::parse(blocked).unwrap()),
            "did not block {blocked}"
        );
    }

    for allowed in [
        "https://example.com/catalogout",
        "https://example.com/removable",
        "https://example.com/deleteful",
        "https://example.com/article?next=/catalogout",
        "https://example.com/article?action=read",
        "https://example.com/article?removed=false",
    ] {
        assert!(
            !is_destructive_url(&Url::parse(allowed).unwrap()),
            "false positive for {allowed}"
        );
    }
}

#[test]
fn destructive_navigation_rejects_nested_encodings_and_structured_action_keys() {
    for blocked in [
        "https://example.com/%256Cogout",
        "https://example.com/account%252Fdelete",
        "https://example.com/account%252d%2564elete",
        "https://example.com/?action=%2564elete",
        "https://example.com/?act=delete",
        "https://example.com/?action[]=delete",
        "https://example.com/?ACTION%255B%255D=%2573ign%252dout",
        "https://example.com/?safe=x%2526act%253Ddelete",
    ] {
        assert!(
            is_destructive_url(&Url::parse(blocked).unwrap()),
            "did not block {blocked}"
        );
    }

    for allowed in [
        "https://example.com/%2563atalogout",
        "https://example.com/?act=read",
        "https://example.com/?action[]=reader",
        "https://example.com/?next=%2563atalogout",
        "https://example.com/?redaction=enabled",
    ] {
        assert!(
            !is_destructive_url(&Url::parse(allowed).unwrap()),
            "false positive for {allowed}"
        );
    }

    let ordered = Url::parse("https://example.com/?b=2&act=read&b=1").unwrap();
    let before = ordered.as_str().to_owned();
    assert!(!is_destructive_url(&ordered));
    assert_eq!(ordered.as_str(), before);

    let deeply_nested = format!(
        "https://example.com/?action=%{}64elete",
        "25".repeat(10_000)
    );
    assert!(is_destructive_url(&Url::parse(&deeply_nested).unwrap()));
}

#[test]
fn destructive_navigation_recognizes_well_formed_action_query_roots() {
    for blocked in [
        "https://example.com/?action[0]=delete",
        "https://example.com/?act[name]=sign-out",
        "https://example.com/?operation[][nested]=remove",
        "https://example.com/?ACTION%5BName%5D=DELETE",
        "https://example.com/?action%255B0%255D=%2564elete",
        "https://example.com/?act%255B%255D%255Bname%255D=remove",
        "https://example.com/?safe=read&action[0]=delete&safe=keep",
    ] {
        assert!(
            is_destructive_url(&Url::parse(blocked).unwrap()),
            "did not block {blocked}"
        );
    }

    for allowed in [
        "https://example.com/?foo[action]=delete",
        "https://example.com/?action[=delete",
        "https://example.com/?action]=delete",
        "https://example.com/?action[0]suffix=delete",
        "https://example.com/?action[nested[0]]=delete",
        "https://example.com/?redaction[0]=delete",
    ] {
        assert!(
            !is_destructive_url(&Url::parse(allowed).unwrap()),
            "false positive for {allowed}"
        );
    }

    let ordered =
        Url::parse("https://example.com/?b=2&action[0]=read&b=1&action[name]=keep").unwrap();
    let before = ordered.as_str().to_owned();
    assert!(!is_destructive_url(&ordered));
    assert_eq!(ordered.as_str(), before);

    let duplicated =
        Url::parse("https://example.com/?action[0]=read&b=2&action[0]=delete&action[0]=read")
            .unwrap();
    assert!(is_destructive_url(&duplicated));

    let linear = format!(
        "https://example.com/?action{}=delete",
        "[component]".repeat(10_000)
    );
    assert!(is_destructive_url(&Url::parse(&linear).unwrap()));
}

#[test]
fn robots_selects_the_most_specific_case_insensitive_group() {
    let policy = RobotsPolicy::parse(
        r#"
            USER-AGENT: *
            Disallow: /

            User-Agent: rsc
            Allow: /less-specific

            user-agent: RSCRAPER
            disallow: /private # inline comment
            allow: /private/public
            CRAWL-DELAY: 1.5
        "#,
        "rscraper",
    );

    assert!(policy.allows(&Url::parse("https://example.com/").unwrap()));
    assert!(!policy.allows(&Url::parse("https://example.com/private/x").unwrap()));
    assert!(policy.allows(&Url::parse("https://example.com/private/public/report").unwrap()));
    assert_eq!(policy.crawl_delay(), Some(Duration::from_millis(1500)));
}

#[test]
fn robots_combines_equal_groups_and_uses_longest_match_with_allow_ties() {
    let policy = RobotsPolicy::parse(
        r#"
            User-agent: rscraper
            Disallow: /same
            Allow: /same
            Disallow: /folder
            Allow: /folder/open

            User-agent: rscraper
            Disallow: /second
            Disallow:
        "#,
        "rscraper/0.2",
    );

    assert!(policy.allows(&Url::parse("https://example.com/same").unwrap()));
    assert!(!policy.allows(&Url::parse("https://example.com/folder/closed").unwrap()));
    assert!(policy.allows(&Url::parse("https://example.com/folder/open/report").unwrap()));
    assert!(!policy.allows(&Url::parse("https://example.com/second").unwrap()));
    assert!(policy.allows(&Url::parse("https://example.com/unlisted").unwrap()));
}

#[test]
fn robots_falls_back_to_star_and_ignores_invalid_delays_and_directives() {
    let policy = RobotsPolicy::parse(
        r#"
            nonsense without a colon
            User-agent: *
            Disallow: /fallback
            Crawl-delay: -2
            Crawl-delay: nan
            Crawl-delay: 10000000000000000000
        "#,
        "rscraper",
    );

    assert!(!policy.allows(&Url::parse("https://example.com/fallback/x").unwrap()));
    assert!(policy.allows(&Url::parse("https://example.com/elsewhere").unwrap()));
    assert_eq!(policy.crawl_delay(), None);
}

#[test]
fn robots_normalizes_bom_groups_product_tokens_and_percent_encoded_octets() {
    let bom = RobotsPolicy::parse(
        "\u{feff}User-agent: *\r\nDisallow: /private\r\n",
        "rscraper",
    );
    assert!(!bom.allows(&Url::parse("https://example.com/private").unwrap()));

    let empty_specific = RobotsPolicy::parse(
        "User-agent: rscraper\n\nUser-agent: *\nDisallow: /private\n",
        "rscraper/0.2",
    );
    assert!(empty_specific.allows(&Url::parse("https://example.com/private").unwrap()));

    let no_suffix_match = RobotsPolicy::parse(
        "User-agent: scraper\nDisallow: /suffix\n\nUser-agent: *\nAllow: /\n",
        "RSCrApEr/0.2",
    );
    assert!(no_suffix_match.allows(&Url::parse("https://example.com/suffix").unwrap()));

    let encoded = RobotsPolicy::parse(
        "User-agent: RSC\n# a comment is not a blank group boundary\nDisallow: /foo%62ar\nDisallow: /x?key=%76alue\n",
        "rscraper/0.2",
    );
    assert!(!encoded.allows(&Url::parse("https://example.com/foobar").unwrap()));
    assert!(!encoded.allows(&Url::parse("https://example.com/foo%62ar/more").unwrap()));
    assert!(!encoded.allows(&Url::parse("https://example.com/x?key=value").unwrap()));
    assert!(!encoded.allows(&Url::parse("https://example.com/x?key=%76alue").unwrap()));
}

#[tokio::test]
async fn crawler_validates_bounds_urls_proxies_and_delay_before_spawning() {
    let crawler = Crawler::new(test_client());
    let valid = crawl_config(Url::parse("http://127.0.0.1/").unwrap());

    for max_pages in [0, 101] {
        let mut invalid = valid.clone();
        invalid.max_pages = max_pages;
        assert!(crawler.stream(invalid).is_err());
    }
    for concurrency in [0, 17] {
        let mut invalid = valid.clone();
        invalid.concurrency = concurrency;
        assert!(crawler.stream(invalid).is_err());
    }
    for start in ["ftp://example.com/", "http://user@example.com/"] {
        let mut invalid = valid.clone();
        invalid.start_url = Url::parse(start).unwrap();
        assert!(crawler.stream(invalid).is_err());
    }
    let mut destructive_start = valid.clone();
    destructive_start.start_url = Url::parse("http://127.0.0.1/account/logout").unwrap();
    assert!(crawler.stream(destructive_start).is_err());

    let mut invalid_proxy = valid.clone();
    invalid_proxy.proxies = vec![Url::parse("ftp://127.0.0.1:8080/").unwrap()];
    assert!(crawler.stream(invalid_proxy).is_err());

    let mut invalid_delay = valid;
    invalid_delay.minimum_delay = Duration::MAX;
    assert!(crawler.stream(invalid_delay).is_err());
}

#[tokio::test]
async fn public_hostname_remote_dns_proxies_are_rejected_before_spawning() {
    let crawler = Crawler::new(FetchClient::builder().build().unwrap());
    let valid = crawl_config(Url::parse("https://example.com/").unwrap());

    for proxy in [
        "http://1.1.1.1:8080/",
        "https://1.1.1.1:8080/",
        "socks4a://1.1.1.1:1080/",
        "socks5h://1.1.1.1:1080/",
    ] {
        let mut invalid = valid.clone();
        invalid.proxies = vec![Url::parse(proxy).unwrap()];
        match crawler.stream(invalid) {
            Err(rscraper_core::Error::Policy(_)) => {}
            Err(error) => panic!("unexpected error for {proxy}: {error}"),
            Ok((stream, _, _)) => {
                drop(stream);
                panic!("accepted {proxy} before spawning");
            }
        }
    }
}

#[tokio::test]
async fn scheduler_respects_limits() {
    let gate = Arc::new(Semaphore::new(0));
    let handler_gate = Arc::clone(&gate);
    let server = ControlledServer::spawn(move |target| {
        if target == "/" {
            FixtureAction::Respond(FixtureResponse::html(
                (0..8)
                    .map(|index| format!("<a href='/child-{index}'>child</a>"))
                    .collect::<String>(),
            ))
        } else {
            FixtureAction::Wait(
                Arc::clone(&handler_gate),
                FixtureResponse::html("<p>leaf</p>"),
            )
        }
    })
    .await;
    let crawler = Crawler::new(test_client());
    let mut config = crawl_config(server.url("/"));
    config.max_pages = 5;
    config.concurrency = 3;
    let (stream, _control, stats) = crawler.stream(config).unwrap();

    server.wait_for_requests(4).await;
    assert_eq!(server.maximum_active(), 3);
    gate.add_permits(8);
    let output = tokio::time::timeout(Duration::from_secs(2), stream.collect::<Vec<_>>())
        .await
        .expect("bounded scheduler did not terminate");

    assert_eq!(output.len(), 5);
    assert!(output.iter().all(Result::is_ok));
    assert_eq!(server.request_targets().len(), 5);
    assert_eq!(server.maximum_active(), 3);
    assert_eq!(
        stats.snapshot(),
        CrawlStatsSnapshot {
            attempted: 5,
            succeeded: 5,
            failed: 0,
            queued: 5,
            skipped: 4,
        }
    );
}

#[tokio::test]
async fn failed_fetches_still_consume_the_max_pages_budget_and_emit_once() {
    let server = ControlledServer::spawn(|target| {
        if target == "/" {
            FixtureAction::Respond(FixtureResponse::html(
                (0..8)
                    .map(|index| format!("<a href='/bad-{index}'>bad</a>"))
                    .collect::<String>(),
            ))
        } else {
            FixtureAction::Respond(FixtureResponse::unsupported())
        }
    })
    .await;
    let crawler = Crawler::new(test_client());
    let mut config = crawl_config(server.url("/"));
    config.max_pages = 4;
    config.concurrency = 4;
    let (output, stats) = collect(&crawler, config).await;

    assert_eq!(output.len(), 4);
    assert_eq!(output.iter().filter(|item| item.is_ok()).count(), 1);
    assert_eq!(output.iter().filter(|item| item.is_err()).count(), 3);
    assert_eq!(server.request_targets().len(), 4);
    assert_eq!(stats.snapshot().attempted, 4);
    assert_eq!(stats.snapshot().failed, 3);
}

#[tokio::test]
async fn normalized_fragment_variants_are_fetched_once() {
    let server = ControlledServer::spawn(|target| match target {
        "/" => FixtureAction::Respond(FixtureResponse::html(
            "<a href='/child#one'>one</a><a href='/child#two'>two</a><a href='/child'>plain</a>",
        )),
        "/child" => FixtureAction::Respond(FixtureResponse::html("leaf")),
        _ => FixtureAction::Respond(FixtureResponse::text(404, "missing")),
    })
    .await;
    let crawler = Crawler::new(test_client());
    let (output, stats) = collect(&crawler, crawl_config(server.url("/"))).await;

    assert_eq!(output.len(), 2);
    assert_eq!(
        server
            .request_targets()
            .iter()
            .filter(|target| target.as_str() == "/child")
            .count(),
        1
    );
    assert_eq!(stats.snapshot().attempted, 2);
}

#[tokio::test]
async fn redirects_emit_and_resolve_links_from_the_final_url() {
    let server = ControlledServer::spawn(|target| match target {
        "/start" => FixtureAction::Respond(FixtureResponse::redirect("/blog/post")),
        "/blog/post" => FixtureAction::Respond(FixtureResponse::html("<a href='child'>child</a>")),
        "/blog/child" => FixtureAction::Respond(FixtureResponse::html("leaf")),
        _ => FixtureAction::Respond(FixtureResponse::text(404, "missing")),
    })
    .await;
    let crawler = Crawler::new(test_client());
    let (output, stats) = collect(&crawler, crawl_config(server.url("/start"))).await;
    let results = output
        .into_iter()
        .collect::<rscraper_core::Result<Vec<_>>>()
        .unwrap();

    assert_eq!(results.len(), 2);
    assert_eq!(results[0].url, server.url("/blog/post"));
    assert_eq!(results[0].links, vec![server.url("/blog/child")]);
    assert_eq!(results[1].url, server.url("/blog/child"));
    assert_eq!(stats.snapshot().attempted, 2);
}

#[tokio::test]
async fn crawler_redirect_continuations_obey_the_fetch_client_hop_limit() {
    let server = ControlledServer::spawn(|target| {
        let hop = target
            .strip_prefix('/')
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(usize::MAX);
        if hop < 11 {
            FixtureAction::Respond(FixtureResponse::redirect(format!("/{}", hop + 1)))
        } else {
            FixtureAction::Respond(FixtureResponse::html("must not fetch"))
        }
    })
    .await;
    let crawler = Crawler::new(test_client());
    let mut config = crawl_config(server.url("/0"));
    config.max_pages = 1;
    let (output, stats) = collect(&crawler, config).await;

    assert_eq!(output.len(), 1);
    assert!(output[0].is_err());
    assert!(!server.request_targets().contains(&"/11".to_owned()));
    assert_eq!(server.request_targets().len(), 11);
    assert_eq!(stats.snapshot().attempted, 1);
    assert_eq!(stats.snapshot().failed, 1);
}

#[tokio::test]
async fn initial_multi_hop_redirect_denial_preserves_typed_start_attempt_provenance() {
    let server = ControlledServer::spawn(|target| match target {
        "/robots.txt" => FixtureAction::Respond(FixtureResponse::text(
            200,
            "User-agent: rscraper\nDisallow: /blocked\n",
        )),
        "/start" => FixtureAction::Respond(FixtureResponse::redirect("/middle")),
        "/middle" => FixtureAction::Respond(FixtureResponse::redirect("/blocked")),
        "/blocked" => {
            FixtureAction::Respond(FixtureResponse::html("blocked-page-body-secret-sentinel"))
        }
        _ => FixtureAction::Respond(FixtureResponse::text(404, "missing")),
    })
    .await;
    let blocked_url = server.url("/blocked");
    let crawler = Crawler::new(test_client());
    let mut config = crawl_config(server.url("/start"));
    config.max_pages = 1;
    config.respect_robots = true;
    let (output, stats) = collect(&crawler, config).await;

    assert_eq!(output.len(), 1);
    assert!(matches!(
        &output[0],
        Err(Error::RobotsDenied(url)) if url == &blocked_url
    ));
    assert_eq!(
        server.request_targets(),
        vec!["/robots.txt", "/start", "/middle"]
    );
    assert!(!format!("{:?}", output[0]).contains("blocked-page-body-secret-sentinel"));
    assert_eq!(stats.snapshot().attempted, 1);
    assert_eq!(stats.snapshot().failed, 1);
    assert_eq!(stats.snapshot().skipped, 1);
}

#[tokio::test]
async fn later_discovered_redirect_denial_is_a_silent_skip_without_target_fetch() {
    let server = ControlledServer::spawn(|target| match target {
        "/robots.txt" => FixtureAction::Respond(FixtureResponse::text(
            200,
            "User-agent: rscraper\nDisallow: /blocked\n",
        )),
        "/" => FixtureAction::Respond(FixtureResponse::html(
            "<a href='/alias'>alias</a><a href='/allowed'>allowed</a>",
        )),
        "/alias" => FixtureAction::Respond(FixtureResponse::redirect("/blocked")),
        "/allowed" => FixtureAction::Respond(FixtureResponse::html("allowed page")),
        "/blocked" => {
            FixtureAction::Respond(FixtureResponse::html("blocked-page-body-secret-sentinel"))
        }
        _ => FixtureAction::Respond(FixtureResponse::text(404, "missing")),
    })
    .await;
    let root_url = server.url("/");
    let allowed_url = server.url("/allowed");
    let crawler = Crawler::new(test_client());
    let mut config = crawl_config(root_url.clone());
    config.max_pages = 3;
    config.concurrency = 1;
    config.respect_robots = true;
    let (output, stats) = collect(&crawler, config).await;

    assert_eq!(output.len(), 2);
    assert!(output.iter().all(Result::is_ok));
    let urls = output
        .into_iter()
        .map(Result::unwrap)
        .map(|result| result.url)
        .collect::<Vec<_>>();
    assert_eq!(urls, vec![root_url, allowed_url]);
    assert_eq!(
        server.request_targets(),
        vec!["/robots.txt", "/", "/alias", "/allowed"]
    );
    assert_eq!(stats.snapshot().attempted, 3);
    assert_eq!(stats.snapshot().failed, 0);
    assert_eq!(stats.snapshot().skipped, 1);
}

#[tokio::test]
async fn cross_origin_redirect_is_rejected_before_target_request_when_scoped() {
    let destination = ControlledServer::spawn(|target| match target {
        "/robots.txt" => FixtureAction::Respond(FixtureResponse::text(
            200,
            "User-agent: rscraper\nDisallow: /\n",
        )),
        "/landing" => FixtureAction::Respond(FixtureResponse::html("must not fetch")),
        _ => FixtureAction::Respond(FixtureResponse::text(404, "missing")),
    })
    .await;
    let landing = destination.url("/landing").to_string();
    let source = ControlledServer::spawn(move |target| match target {
        "/robots.txt" => FixtureAction::Respond(FixtureResponse::text(
            200,
            "User-agent: rscraper\nAllow: /\n",
        )),
        "/start" => FixtureAction::Respond(FixtureResponse::redirect(landing.clone())),
        _ => FixtureAction::Respond(FixtureResponse::text(404, "missing")),
    })
    .await;
    let crawler = Crawler::new(test_client());
    let mut config = crawl_config(source.url("/start"));
    config.respect_robots = true;
    let (output, _) = collect(&crawler, config).await;

    assert_eq!(output.len(), 1);
    assert!(output[0].is_err());
    assert!(destination.request_targets().is_empty());
}

#[tokio::test]
async fn unrestricted_redirect_fetches_target_robots_before_landing() {
    let destination = ControlledServer::spawn(|target| match target {
        "/robots.txt" => FixtureAction::Respond(FixtureResponse::text(
            200,
            "User-agent: rscraper\nDisallow: /landing\n",
        )),
        "/landing" => FixtureAction::Respond(FixtureResponse::html("must not fetch")),
        _ => FixtureAction::Respond(FixtureResponse::text(404, "missing")),
    })
    .await;
    let landing = destination.url("/landing").to_string();
    let source = ControlledServer::spawn(move |target| match target {
        "/robots.txt" => FixtureAction::Respond(FixtureResponse::text(
            200,
            "User-agent: rscraper\nAllow: /\n",
        )),
        "/start" => FixtureAction::Respond(FixtureResponse::redirect(landing.clone())),
        _ => FixtureAction::Respond(FixtureResponse::text(404, "missing")),
    })
    .await;
    let crawler = Crawler::new(test_client());
    let mut config = crawl_config(source.url("/start"));
    config.same_origin_only = false;
    config.respect_robots = true;
    let (output, _) = collect(&crawler, config).await;

    assert_eq!(output.len(), 1);
    assert!(output[0].is_err());
    assert_eq!(
        destination.request_targets(),
        vec!["/robots.txt".to_owned()]
    );
}

#[tokio::test]
async fn unrestricted_redirect_proceeds_after_target_robots_and_uses_final_url_as_base() {
    let destination = ControlledServer::spawn(|target| match target {
        "/robots.txt" => FixtureAction::Respond(FixtureResponse::text(
            200,
            "User-agent: rscraper\nAllow: /\n",
        )),
        "/folder/landing" => {
            FixtureAction::Respond(FixtureResponse::html("<a href='child'>child</a>"))
        }
        "/folder/child" => FixtureAction::Respond(FixtureResponse::html("leaf")),
        _ => FixtureAction::Respond(FixtureResponse::text(404, "missing")),
    })
    .await;
    let landing = destination.url("/folder/landing").to_string();
    let source = ControlledServer::spawn(move |target| match target {
        "/robots.txt" => FixtureAction::Respond(FixtureResponse::text(
            200,
            "User-agent: rscraper\nAllow: /\n",
        )),
        "/start" => FixtureAction::Respond(FixtureResponse::redirect(landing.clone())),
        _ => FixtureAction::Respond(FixtureResponse::text(404, "missing")),
    })
    .await;
    let crawler = Crawler::new(test_client());
    let mut config = crawl_config(source.url("/start"));
    config.same_origin_only = false;
    config.respect_robots = true;
    let (output, _) = collect(&crawler, config).await;
    let results = output
        .into_iter()
        .collect::<rscraper_core::Result<Vec<_>>>()
        .unwrap();

    assert_eq!(results.len(), 2);
    assert_eq!(results[0].url, destination.url("/folder/landing"));
    assert_eq!(results[0].links, vec![destination.url("/folder/child")]);
    assert_eq!(
        destination.request_targets(),
        vec![
            "/robots.txt".to_owned(),
            "/folder/landing".to_owned(),
            "/folder/child".to_owned()
        ]
    );
    assert!(destination.requests().iter().all(|request| {
        request.headers.get("user-agent").map(String::as_str) == Some(CRAWLER_USER_AGENT)
    }));
}

#[tokio::test]
async fn temporary_empty_frontier_waits_for_in_flight_discoveries() {
    let gate = Arc::new(Semaphore::new(0));
    let handler_gate = Arc::clone(&gate);
    let server = ControlledServer::spawn(move |target| match target {
        "/" => FixtureAction::Respond(FixtureResponse::html(
            "<a href='/slow-a'>a</a><a href='/slow-b'>b</a>",
        )),
        "/slow-a" => FixtureAction::Wait(
            Arc::clone(&handler_gate),
            FixtureResponse::html("<a href='/child-a'>child a</a>"),
        ),
        "/slow-b" => FixtureAction::Wait(
            Arc::clone(&handler_gate),
            FixtureResponse::html("<a href='/child-b'>child b</a>"),
        ),
        "/child-a" | "/child-b" => FixtureAction::Respond(FixtureResponse::html("leaf")),
        _ => FixtureAction::Respond(FixtureResponse::text(404, "missing")),
    })
    .await;
    let crawler = Crawler::new(test_client());
    let mut config = crawl_config(server.url("/"));
    config.concurrency = 2;
    let (stream, _control, stats) = crawler.stream(config).unwrap();

    server.wait_for_requests(3).await;
    gate.add_permits(2);
    let output = stream.collect::<Vec<_>>().await;

    assert_eq!(output.len(), 5);
    assert!(output.iter().all(Result::is_ok));
    assert!(server.request_targets().contains(&"/child-a".to_owned()));
    assert!(server.request_targets().contains(&"/child-b".to_owned()));
    assert_eq!(stats.snapshot().attempted, 5);
}

#[tokio::test]
async fn proxies_rotate_by_attempt_index_without_a_direct_request() {
    let target_root = "http://crawl.invalid/".to_owned();
    let target_a = "http://crawl.invalid/a".to_owned();
    let target_b = "http://crawl.invalid/b".to_owned();
    let proxy_one_root = target_root.clone();
    let proxy_one_b = target_b.clone();
    let proxy_one = ControlledServer::spawn(move |target| {
        if target == proxy_one_root {
            FixtureAction::Respond(FixtureResponse::html(
                "<a href='/a'>a</a><a href='/b'>b</a>",
            ))
        } else if target == proxy_one_b {
            FixtureAction::Respond(FixtureResponse::html("b"))
        } else {
            FixtureAction::Respond(FixtureResponse::text(500, "wrong proxy"))
        }
    })
    .await;
    let proxy_two_a = target_a.clone();
    let proxy_two = ControlledServer::spawn(move |target| {
        if target == proxy_two_a {
            FixtureAction::Respond(FixtureResponse::html("a"))
        } else {
            FixtureAction::Respond(FixtureResponse::text(500, "wrong proxy"))
        }
    })
    .await;
    let crawler = Crawler::new(test_client());
    let mut config = crawl_config(Url::parse(&target_root).unwrap());
    config.max_pages = 3;
    config.concurrency = 1;
    config.proxies = vec![proxy_one.proxy_url(), proxy_two.proxy_url()];
    let (output, _) = collect(&crawler, config).await;

    assert!(output.iter().all(Result::is_ok));
    assert_eq!(proxy_one.request_targets(), vec![target_root, target_b]);
    assert_eq!(proxy_two.request_targets(), vec![target_a]);
}

#[tokio::test]
async fn pause_blocks_only_new_scheduling_and_resume_wakes_the_frontier() {
    let gate = Arc::new(Semaphore::new(0));
    let handler_gate = Arc::clone(&gate);
    let server = ControlledServer::spawn(move |target| match target {
        "/" => FixtureAction::Wait(
            Arc::clone(&handler_gate),
            FixtureResponse::html("<a href='/a'>a</a><a href='/b'>b</a>"),
        ),
        "/a" | "/b" => FixtureAction::Respond(FixtureResponse::html("leaf")),
        _ => FixtureAction::Respond(FixtureResponse::text(404, "missing")),
    })
    .await;
    let crawler = Crawler::new(test_client());
    let (mut stream, control, stats) = crawler.stream(crawl_config(server.url("/"))).unwrap();
    let initial = stats.snapshot();

    server.wait_for_requests(1).await;
    control.set_paused(true).unwrap();
    gate.add_permits(1);
    assert!(stream.next().await.unwrap().is_ok());
    tokio::task::yield_now().await;

    let paused = stats.snapshot();
    assert_monotonic(initial, paused);
    assert_eq!(paused.attempted, 1);
    assert_eq!(server.request_targets(), vec!["/".to_owned()]);

    control.set_paused(false).unwrap();
    let remaining = stream.collect::<Vec<_>>().await;
    assert_eq!(remaining.len(), 2);
    assert!(remaining.iter().all(Result::is_ok));
    assert_eq!(stats.snapshot().attempted, 3);
}

#[tokio::test]
async fn pause_also_blocks_a_new_robots_redirect_hop() {
    let gate = Arc::new(Semaphore::new(0));
    let robots_gate = Arc::clone(&gate);
    let server = ControlledServer::spawn(move |target| match target {
        "/robots.txt" => FixtureAction::Wait(
            Arc::clone(&robots_gate),
            FixtureResponse::redirect("/rules.txt"),
        ),
        "/rules.txt" => FixtureAction::Respond(FixtureResponse::text(
            200,
            "User-agent: rscraper\nAllow: /\n",
        )),
        "/" => FixtureAction::Respond(FixtureResponse::html("leaf")),
        _ => FixtureAction::Respond(FixtureResponse::text(404, "missing")),
    })
    .await;
    let crawler = Crawler::new(test_client());
    let mut config = crawl_config(server.url("/"));
    config.respect_robots = true;
    let (stream, control, _) = crawler.stream(config).unwrap();

    server.wait_for_requests(1).await;
    control.set_paused(true).unwrap();
    gate.add_permits(1);
    server.wait_for_idle().await;
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    assert_eq!(server.request_targets(), vec!["/robots.txt".to_owned()]);

    control.set_paused(false).unwrap();
    let output = stream.collect::<Vec<_>>().await;
    assert_eq!(output.len(), 1);
    assert!(output[0].is_ok());
    assert_eq!(
        server.request_targets(),
        vec![
            "/robots.txt".to_owned(),
            "/rules.txt".to_owned(),
            "/".to_owned()
        ]
    );
}

#[tokio::test]
async fn cancellation_closes_the_stream_and_reaps_in_flight_fetches() {
    let gate = Arc::new(Semaphore::new(0));
    let handler_gate = Arc::clone(&gate);
    let server = ControlledServer::spawn(move |_| {
        FixtureAction::Wait(
            Arc::clone(&handler_gate),
            FixtureResponse::html("never delivered"),
        )
    })
    .await;
    let crawler = Crawler::new(test_client());
    let (stream, control, stats) = crawler.stream(crawl_config(server.url("/"))).unwrap();

    server.wait_for_requests(1).await;
    assert_eq!(server.active(), 1);
    control.cancel();
    let output = tokio::time::timeout(Duration::from_secs(1), stream.collect::<Vec<_>>())
        .await
        .expect("cancelled crawl did not close");
    server.wait_for_idle().await;

    assert!(output.is_empty());
    assert_eq!(server.active(), 0);
    assert_eq!(stats.snapshot().attempted, 1);
}

#[tokio::test]
async fn dropping_the_result_stream_cancels_and_reaps_the_scheduler() {
    let gate = Arc::new(Semaphore::new(0));
    let handler_gate = Arc::clone(&gate);
    let server = ControlledServer::spawn(move |_| {
        FixtureAction::Wait(
            Arc::clone(&handler_gate),
            FixtureResponse::html("never delivered"),
        )
    })
    .await;
    let crawler = Crawler::new(test_client());
    let (stream, control, _) = crawler.stream(crawl_config(server.url("/"))).unwrap();

    server.wait_for_requests(1).await;
    drop(stream);
    server.wait_for_idle().await;

    assert!(control.cancellation.is_cancelled());
    assert_eq!(server.active(), 0);
}

#[tokio::test]
async fn successful_non_html_pages_are_emitted_without_link_discovery() {
    let server = ControlledServer::spawn(|target| match target {
        "/" => FixtureAction::Respond(FixtureResponse::json(
            r#"{"html":"<a href='/hidden'>not a document link</a>"}"#,
        )),
        "/hidden" => FixtureAction::Respond(FixtureResponse::html("must not fetch")),
        _ => FixtureAction::Respond(FixtureResponse::text(404, "missing")),
    })
    .await;
    let crawler = Crawler::new(test_client());
    let (output, stats) = collect(&crawler, crawl_config(server.url("/"))).await;
    let result = output.into_iter().next().unwrap().unwrap();

    assert!(result.links.is_empty());
    assert_eq!(server.request_targets(), vec!["/".to_owned()]);
    assert_eq!(stats.snapshot().succeeded, 1);
}

#[tokio::test]
async fn successful_xhtml_pages_contribute_links() {
    let server = ControlledServer::spawn(|target| match target {
        "/" => FixtureAction::Respond(FixtureResponse {
            status: 200,
            content_type: "application/xhtml+xml; charset=utf-8",
            headers: Vec::new(),
            body: b"<html><body><a href='/child'>child</a></body></html>".to_vec(),
        }),
        "/child" => FixtureAction::Respond(FixtureResponse::html("leaf")),
        _ => FixtureAction::Respond(FixtureResponse::text(404, "missing")),
    })
    .await;
    let crawler = Crawler::new(test_client());
    let (output, stats) = collect(&crawler, crawl_config(server.url("/"))).await;

    assert_eq!(output.len(), 2);
    assert!(output.iter().all(Result::is_ok));
    assert!(server.request_targets().contains(&"/child".to_owned()));
    assert_eq!(stats.snapshot().succeeded, 2);
}

#[tokio::test]
async fn non_success_html_is_emitted_without_link_discovery() {
    let server = ControlledServer::spawn(|target| match target {
        "/" => FixtureAction::Respond(FixtureResponse {
            status: 404,
            content_type: "text/html; charset=utf-8",
            headers: Vec::new(),
            body: b"<a href='/hidden'>hidden</a>".to_vec(),
        }),
        "/hidden" => FixtureAction::Respond(FixtureResponse::html("must not fetch")),
        _ => FixtureAction::Respond(FixtureResponse::text(404, "missing")),
    })
    .await;
    let crawler = Crawler::new(test_client());
    let (output, stats) = collect(&crawler, crawl_config(server.url("/"))).await;
    let result = output.into_iter().next().unwrap().unwrap();

    assert_eq!(result.status, 404);
    assert!(result.links.is_empty());
    assert_eq!(server.request_targets(), vec!["/".to_owned()]);
    assert_eq!(stats.snapshot().succeeded, 1);
}

#[tokio::test]
async fn robots_is_cached_once_and_denied_urls_increment_skipped() {
    let server = ControlledServer::spawn(|target| match target {
        "/robots.txt" => FixtureAction::Respond(FixtureResponse::text(
            200,
            "User-agent: rscraper\nDisallow: /private\nAllow: /private/public\n",
        )),
        "/" => FixtureAction::Respond(FixtureResponse::html(
            "<a href='/private'>private</a><a href='/allowed'>allowed</a>",
        )),
        "/allowed" => FixtureAction::Respond(FixtureResponse::html(
            "<a href='/private/public'>public</a>",
        )),
        "/private/public" => FixtureAction::Respond(FixtureResponse::html("leaf")),
        "/private" => FixtureAction::Respond(FixtureResponse::html("must not fetch")),
        _ => FixtureAction::Respond(FixtureResponse::text(404, "missing")),
    })
    .await;
    let crawler = Crawler::new(test_client());
    let mut config = crawl_config(server.url("/"));
    config.respect_robots = true;
    let (output, stats) = collect(&crawler, config).await;
    let targets = server.request_targets();

    assert_eq!(output.len(), 3);
    assert!(output.iter().all(Result::is_ok));
    assert_eq!(
        targets
            .iter()
            .filter(|target| target.as_str() == "/robots.txt")
            .count(),
        1
    );
    assert!(!targets.contains(&"/private".to_owned()));
    assert!(stats.snapshot().skipped >= 1);
}

#[tokio::test]
async fn denied_start_url_emits_typed_robots_denied_without_fetching_the_page() {
    let server = ControlledServer::spawn(|target| match target {
        "/robots.txt" => FixtureAction::Respond(FixtureResponse::text(
            200,
            "User-agent: rscraper\nDisallow: /blocked\n",
        )),
        "/blocked" => FixtureAction::Respond(FixtureResponse::html(
            "start-page-body-must-never-be-requested",
        )),
        _ => FixtureAction::Respond(FixtureResponse::text(404, "missing")),
    })
    .await;
    let start_url = server.url("/blocked");
    let crawler = Crawler::new(test_client());
    let mut config = crawl_config(start_url.clone());
    config.respect_robots = true;
    let (output, stats) = collect(&crawler, config).await;

    assert_eq!(output.len(), 1);
    assert!(matches!(
        &output[0],
        Err(Error::RobotsDenied(url)) if url == &start_url
    ));
    assert_eq!(server.request_targets(), vec!["/robots.txt"]);
    assert_eq!(stats.snapshot().attempted, 0);
    assert_eq!(stats.snapshot().skipped, 1);
}

#[tokio::test]
async fn non_success_robots_status_emits_typed_http_status_without_fetching_the_page() {
    let server = ControlledServer::spawn(|target| match target {
        "/robots.txt" => {
            FixtureAction::Respond(FixtureResponse::text(500, "private-robots-error-body"))
        }
        "/" => FixtureAction::Respond(FixtureResponse::html(
            "start-page-body-must-never-be-requested",
        )),
        _ => FixtureAction::Respond(FixtureResponse::text(404, "missing")),
    })
    .await;
    let robots_url = server.url("/robots.txt");
    let crawler = Crawler::new(test_client());
    let mut config = crawl_config(server.url("/"));
    config.respect_robots = true;
    let (output, stats) = collect(&crawler, config).await;

    assert_eq!(output.len(), 1);
    assert!(matches!(
        &output[0],
        Err(Error::HttpStatus { status: 500, url }) if url == &robots_url
    ));
    let diagnostic = format!("{:?} {}", output[0], output[0].as_ref().unwrap_err());
    assert!(!diagnostic.contains("private-robots-error-body"));
    assert_eq!(server.request_targets(), vec!["/robots.txt"]);
    assert_eq!(stats.snapshot().attempted, 0);
    assert_eq!(stats.snapshot().skipped, 1);
}

#[tokio::test]
async fn crawler_uses_the_same_rs_scraper_user_agent_for_robots_and_pages() {
    let server = ControlledServer::spawn(|target| match target {
        "/robots.txt" => FixtureAction::Respond(FixtureResponse::text(
            200,
            "User-agent: rscraper\nAllow: /\n",
        )),
        "/" => FixtureAction::Respond(FixtureResponse::html("leaf")),
        _ => FixtureAction::Respond(FixtureResponse::text(404, "missing")),
    })
    .await;
    let crawler = Crawler::new(test_client());
    let mut config = crawl_config(server.url("/"));
    config.respect_robots = true;
    let (output, _) = collect(&crawler, config).await;

    assert!(output[0].is_ok());
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    for request in requests {
        assert_eq!(
            request.headers.get("user-agent").map(String::as_str),
            Some(CRAWLER_USER_AGENT),
            "{} advertised a different identity",
            request.target
        );
    }
}

#[tokio::test]
async fn denied_candidates_do_not_starve_later_allowed_links() {
    let server = ControlledServer::spawn(|target| match target {
        "/robots.txt" => FixtureAction::Respond(FixtureResponse::text(
            200,
            "User-agent: rscraper\nDisallow: /deny\n",
        )),
        "/" => FixtureAction::Respond(FixtureResponse::html(
            "<a href='/deny-0'>deny</a><a href='/deny-1'>deny</a><a href='/deny-0'>duplicate</a><a href='/allowed'>allowed</a>",
        )),
        "/allowed" => FixtureAction::Respond(FixtureResponse::html("leaf")),
        path if path.starts_with("/deny") => {
            FixtureAction::Respond(FixtureResponse::html("must not fetch"))
        }
        _ => FixtureAction::Respond(FixtureResponse::text(404, "missing")),
    })
    .await;
    let crawler = Crawler::new(test_client());
    let mut config = crawl_config(server.url("/"));
    config.max_pages = 3;
    config.respect_robots = true;
    let (output, stats) = collect(&crawler, config).await;

    assert_eq!(output.len(), 2);
    assert!(output.iter().all(Result::is_ok));
    assert!(server.request_targets().contains(&"/allowed".to_owned()));
    assert!(!server
        .request_targets()
        .iter()
        .any(|target| target.starts_with("/deny")));
    assert_eq!(stats.snapshot().attempted, 2);
    assert!(stats.snapshot().skipped >= 2);
}

#[tokio::test]
async fn failed_origin_candidates_release_frontier_capacity_for_later_links() {
    let failed = ControlledServer::spawn(|target| match target {
        "/robots.txt" => FixtureAction::Respond(FixtureResponse::unsupported()),
        _ => FixtureAction::Respond(FixtureResponse::html("must not fetch")),
    })
    .await;
    let failed_a = failed.url("/a");
    let failed_b = failed.url("/b");
    let root = ControlledServer::spawn(move |target| match target {
        "/robots.txt" => FixtureAction::Respond(FixtureResponse::text(
            200,
            "User-agent: rscraper\nAllow: /\n",
        )),
        "/" => FixtureAction::Respond(FixtureResponse::html(format!(
            "<a href='{failed_a}'>bad a</a><a href='{failed_b}'>bad b</a><a href='/allowed'>allowed</a>"
        ))),
        "/allowed" => FixtureAction::Respond(FixtureResponse::html("leaf")),
        _ => FixtureAction::Respond(FixtureResponse::text(404, "missing")),
    })
    .await;
    let crawler = Crawler::new(test_client());
    let mut config = crawl_config(root.url("/"));
    config.max_pages = 3;
    config.concurrency = 1;
    config.same_origin_only = false;
    config.respect_robots = true;
    let (output, stats) = collect(&crawler, config).await;

    assert!(output.iter().any(Result::is_err));
    assert!(root.request_targets().contains(&"/allowed".to_owned()));
    assert_eq!(stats.snapshot().attempted, 2);
    assert_eq!(
        failed
            .request_targets()
            .iter()
            .filter(|target| target.as_str() == "/robots.txt")
            .count(),
        1
    );
    assert!(!failed
        .request_targets()
        .iter()
        .any(|target| target == "/a" || target == "/b"));
}

#[tokio::test]
async fn missing_robots_allows_the_origin_and_is_cached() {
    let server = ControlledServer::spawn(|target| match target {
        "/robots.txt" => FixtureAction::Respond(FixtureResponse::text(404, "missing")),
        "/" => FixtureAction::Respond(FixtureResponse::html("<a href='/child'>child</a>")),
        "/child" => FixtureAction::Respond(FixtureResponse::html("leaf")),
        _ => FixtureAction::Respond(FixtureResponse::text(404, "missing")),
    })
    .await;
    let crawler = Crawler::new(test_client());
    let mut config = crawl_config(server.url("/"));
    config.respect_robots = true;
    let (output, stats) = collect(&crawler, config).await;

    assert_eq!(output.len(), 2);
    assert!(output.iter().all(Result::is_ok));
    assert_eq!(
        server
            .request_targets()
            .iter()
            .filter(|target| target.as_str() == "/robots.txt")
            .count(),
        1
    );
    assert_eq!(stats.snapshot().skipped, 0);
}

#[tokio::test]
async fn headerless_robots_404_allows_without_decoding_its_body() {
    let server = ControlledServer::spawn(|target| match target {
        "/robots.txt" => FixtureAction::Respond(FixtureResponse::headerless(404, vec![0xff; 128])),
        "/" => FixtureAction::Respond(FixtureResponse::html("<a href='/child'>child</a>")),
        "/child" => FixtureAction::Respond(FixtureResponse::html("leaf")),
        _ => FixtureAction::Respond(FixtureResponse::text(404, "missing")),
    })
    .await;
    let crawler = Crawler::new(test_client());
    let mut config = crawl_config(server.url("/"));
    config.respect_robots = true;
    let (output, stats) = collect(&crawler, config).await;

    assert_eq!(output.len(), 2);
    assert!(output.iter().all(Result::is_ok));
    assert_eq!(stats.snapshot().failed, 0);
}

#[tokio::test]
async fn robots_transport_failure_fails_closed_and_emits_one_error() {
    let server = ControlledServer::spawn(|target| match target {
        "/robots.txt" => FixtureAction::Respond(FixtureResponse::unsupported()),
        "/" => FixtureAction::Respond(FixtureResponse::html("must not fetch")),
        _ => FixtureAction::Respond(FixtureResponse::text(404, "missing")),
    })
    .await;
    let crawler = Crawler::new(test_client());
    let mut config = crawl_config(server.url("/"));
    config.respect_robots = true;
    let (output, stats) = collect(&crawler, config).await;

    assert_eq!(output.len(), 1);
    assert!(output[0].is_err());
    assert_eq!(server.request_targets(), vec!["/robots.txt".to_owned()]);
    assert_eq!(stats.snapshot().attempted, 0);
    assert_eq!(stats.snapshot().skipped, 1);
}

#[tokio::test]
async fn maximum_of_minimum_and_robots_delay_separates_same_origin_starts() {
    let server = ControlledServer::spawn(|target| match target {
        "/robots.txt" => FixtureAction::Respond(FixtureResponse::text(
            200,
            "User-agent: rscraper\nCrawl-delay: 0.005\n",
        )),
        "/" => FixtureAction::Respond(FixtureResponse::html("<a href='/next'>next</a>")),
        "/next" => FixtureAction::Respond(FixtureResponse::html("leaf")),
        _ => FixtureAction::Respond(FixtureResponse::text(404, "missing")),
    })
    .await;
    let crawler = Crawler::new(test_client());
    let mut config = crawl_config(server.url("/"));
    config.respect_robots = true;
    config.minimum_delay = Duration::from_millis(30);
    config.concurrency = 2;
    let (output, _) = collect(&crawler, config).await;
    let page_starts = server
        .requests()
        .into_iter()
        .filter(|request| request.target != "/robots.txt")
        .collect::<Vec<_>>();

    assert_eq!(output.len(), 2);
    assert_eq!(page_starts.len(), 2);
    assert!(
        page_starts[1]
            .started_at
            .duration_since(page_starts[0].started_at)
            >= Duration::from_millis(25)
    );
}

#[tokio::test]
async fn delayed_origin_does_not_hold_an_unrelated_origin() {
    let other =
        ControlledServer::spawn(|_| FixtureAction::Respond(FixtureResponse::html("other origin")))
            .await;
    let other_url = other.url("/other");
    let linked_other = other_url.to_string();
    let first = ControlledServer::spawn(move |target| match target {
        "/" => FixtureAction::Respond(FixtureResponse::html(format!(
            "<a href='/same'>same</a><a href='{linked_other}'>other</a>"
        ))),
        "/same" => FixtureAction::Respond(FixtureResponse::html("same origin")),
        _ => FixtureAction::Respond(FixtureResponse::text(404, "missing")),
    })
    .await;
    let crawler = Crawler::new(test_client());
    let mut config = crawl_config(first.url("/"));
    config.same_origin_only = false;
    config.minimum_delay = Duration::from_millis(40);
    config.concurrency = 2;
    let (output, _) = collect(&crawler, config).await;

    assert_eq!(output.len(), 3);
    let same_start = first
        .requests()
        .into_iter()
        .find(|request| request.target == "/same")
        .unwrap()
        .started_at;
    let other_start = other.requests()[0].started_at;
    assert!(other_start < same_start);
}

#[tokio::test]
async fn each_fetch_error_is_emitted_once() {
    let server = ControlledServer::spawn(|target| match target {
        "/" => FixtureAction::Respond(FixtureResponse::html(
            "<a href='/bad-a'>a</a><a href='/bad-b'>b</a>",
        )),
        "/bad-a" | "/bad-b" => FixtureAction::Respond(FixtureResponse::unsupported()),
        _ => FixtureAction::Respond(FixtureResponse::text(404, "missing")),
    })
    .await;
    let crawler = Crawler::new(test_client());
    let (output, stats) = collect(&crawler, crawl_config(server.url("/"))).await;

    assert_eq!(output.len(), 3);
    assert_eq!(output.iter().filter(|item| item.is_err()).count(), 2);
    assert_eq!(stats.snapshot().failed, 2);
    for target in ["/bad-a", "/bad-b"] {
        assert_eq!(
            server
                .request_targets()
                .iter()
                .filter(|observed| observed.as_str() == target)
                .count(),
            1
        );
    }
}

#[tokio::test]
async fn hostile_pages_have_a_bounded_result_and_discovery_link_count() {
    const LINK_COUNT: usize = 20_000;

    let body = (0..LINK_COUNT)
        .map(|index| format!("<a href='/item-{index}'>item</a>"))
        .collect::<String>();
    assert!(body.len() < test_client().limits().max_body_bytes);
    let server = ControlledServer::spawn(move |target| match target {
        "/" => FixtureAction::Respond(FixtureResponse::html(body.clone())),
        _ => FixtureAction::Respond(FixtureResponse::html("must not fetch")),
    })
    .await;
    let crawler = Crawler::new(test_client());
    let mut config = crawl_config(server.url("/"));
    config.max_pages = 1;
    let (output, stats) = collect(&crawler, config).await;
    let result = output.into_iter().next().unwrap().unwrap();

    assert_eq!(result.links.len(), MAX_LINKS_PER_PAGE);
    assert_eq!(server.request_targets(), vec!["/".to_owned()]);
    assert_eq!(stats.snapshot().queued, 1);
    assert_eq!(stats.snapshot().skipped, LINK_COUNT as u64);
}

#[tokio::test]
async fn page_redirect_chain_uses_one_absolute_request_deadline() {
    let server = ControlledServer::spawn(|target| match target {
        "/start" => FixtureAction::Delay(
            Duration::from_millis(70),
            FixtureResponse::redirect("/middle"),
        ),
        "/middle" => FixtureAction::Delay(
            Duration::from_millis(70),
            FixtureResponse::redirect("/landing"),
        ),
        "/landing" => FixtureAction::Respond(FixtureResponse::html("too late")),
        _ => FixtureAction::Respond(FixtureResponse::text(404, "missing")),
    })
    .await;
    let crawler = Crawler::new(test_client_with_timeout(Duration::from_millis(120)));
    let mut config = crawl_config(server.url("/start"));
    config.max_pages = 1;
    let (output, stats) = collect(&crawler, config).await;

    assert_eq!(output.len(), 1);
    assert!(output[0].is_err(), "redirect chain escaped its deadline");
    assert!(!server.request_targets().contains(&"/landing".to_owned()));
    assert_eq!(stats.snapshot().attempted, 1);
    assert_eq!(stats.snapshot().failed, 1);
}

#[tokio::test]
async fn robots_redirect_chain_uses_its_own_absolute_request_deadline() {
    let server = ControlledServer::spawn(|target| match target {
        "/robots.txt" => FixtureAction::Delay(
            Duration::from_millis(70),
            FixtureResponse::redirect("/rules-a"),
        ),
        "/rules-a" => FixtureAction::Delay(
            Duration::from_millis(70),
            FixtureResponse::redirect("/rules-b"),
        ),
        "/rules-b" => FixtureAction::Respond(FixtureResponse::text(
            200,
            "User-agent: rscraper\nAllow: /\n",
        )),
        "/" => FixtureAction::Respond(FixtureResponse::html("must not fetch")),
        _ => FixtureAction::Respond(FixtureResponse::text(404, "missing")),
    })
    .await;
    let crawler = Crawler::new(test_client_with_timeout(Duration::from_millis(120)));
    let mut config = crawl_config(server.url("/"));
    config.respect_robots = true;
    let (output, stats) = collect(&crawler, config).await;

    assert_eq!(output.len(), 1);
    assert!(output[0].is_err(), "robots chain escaped its deadline");
    assert_eq!(
        server.request_targets(),
        vec!["/robots.txt".to_owned(), "/rules-a".to_owned()]
    );
    assert_eq!(stats.snapshot().attempted, 0);
    assert_eq!(stats.snapshot().failed, 0);
    assert_eq!(stats.snapshot().skipped, 1);
}

#[tokio::test]
async fn pause_does_not_reset_an_almost_expired_redirect_deadline() {
    let gate = Arc::new(Semaphore::new(0));
    let handler_gate = Arc::clone(&gate);
    let server = ControlledServer::spawn(move |target| match target {
        "/start" => FixtureAction::Wait(
            Arc::clone(&handler_gate),
            FixtureResponse::redirect("/landing"),
        ),
        "/landing" => FixtureAction::Respond(FixtureResponse::html("too late")),
        _ => FixtureAction::Respond(FixtureResponse::text(404, "missing")),
    })
    .await;
    let crawler = Crawler::new(test_client_with_timeout(Duration::from_millis(80)));
    let mut config = crawl_config(server.url("/start"));
    config.max_pages = 1;
    let (stream, control, stats) = crawler.stream(config).unwrap();

    server.wait_for_requests(1).await;
    control.set_paused(true).unwrap();
    gate.add_permits(1);
    server.wait_for_idle().await;
    tokio::time::sleep(Duration::from_millis(120)).await;
    control.set_paused(false).unwrap();
    let output = tokio::time::timeout(Duration::from_secs(2), stream.collect::<Vec<_>>())
        .await
        .expect("crawler did not terminate after resume");

    assert_eq!(output.len(), 1);
    assert!(output[0].is_err());
    assert_eq!(server.request_targets(), vec!["/start".to_owned()]);
    assert_eq!(stats.snapshot().failed, 1);
}

#[tokio::test]
async fn cancellation_reaps_a_page_with_an_active_absolute_deadline() {
    let gate = Arc::new(Semaphore::new(0));
    let handler_gate = Arc::clone(&gate);
    let server = ControlledServer::spawn(move |target| match target {
        "/start" => FixtureAction::Wait(
            Arc::clone(&handler_gate),
            FixtureResponse::redirect("/landing"),
        ),
        "/landing" => FixtureAction::Respond(FixtureResponse::html("must not fetch")),
        _ => FixtureAction::Respond(FixtureResponse::text(404, "missing")),
    })
    .await;
    let crawler = Crawler::new(test_client_with_timeout(Duration::from_millis(120)));
    let (stream, control, _stats) = crawler.stream(crawl_config(server.url("/start"))).unwrap();

    server.wait_for_requests(1).await;
    control.cancel();
    let output = tokio::time::timeout(Duration::from_secs(2), stream.collect::<Vec<_>>())
        .await
        .expect("cancelled crawler did not terminate");
    server.wait_for_idle().await;

    assert!(output.is_empty());
    assert_eq!(server.active(), 0);
    assert_eq!(server.request_targets(), vec!["/start".to_owned()]);
}

#[tokio::test]
async fn direct_reservation_owns_a_redirect_landing_exactly_once() {
    let target_gate = Arc::new(Semaphore::new(0));
    let handler_gate = Arc::clone(&target_gate);
    let server = ControlledServer::spawn(move |target| match target {
        "/" => FixtureAction::Respond(FixtureResponse::html(
            "<a href='/alias'>alias</a><a href='/target'>target</a>",
        )),
        "/alias" => FixtureAction::Respond(FixtureResponse::redirect("/target")),
        "/target" => {
            FixtureAction::Wait(Arc::clone(&handler_gate), FixtureResponse::html("landing"))
        }
        _ => FixtureAction::Respond(FixtureResponse::text(404, "missing")),
    })
    .await;
    let crawler = Crawler::new(test_client());
    let mut config = crawl_config(server.url("/"));
    config.max_pages = 3;
    config.concurrency = 2;
    let (stream, _control, stats) = crawler.stream(config).unwrap();

    server.wait_for_requests(3).await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(
        server
            .request_targets()
            .iter()
            .filter(|target| target.as_str() == "/target")
            .count(),
        1,
        "direct and redirect reservations both fetched the landing"
    );
    target_gate.add_permits(1);
    let output = stream.collect::<Vec<_>>().await;

    assert_eq!(output.len(), 2);
    assert!(output.iter().all(Result::is_ok));
    assert_eq!(
        stats.snapshot(),
        CrawlStatsSnapshot {
            attempted: 3,
            succeeded: 2,
            failed: 0,
            queued: 3,
            skipped: 1,
        }
    );
}

#[tokio::test]
async fn competing_redirects_claim_one_canonical_landing() {
    let target_gate = Arc::new(Semaphore::new(0));
    let handler_gate = Arc::clone(&target_gate);
    let server = ControlledServer::spawn(move |target| match target {
        "/" => FixtureAction::Respond(FixtureResponse::html(
            "<a href='/a'>a</a><a href='/b'>b</a>",
        )),
        "/a" | "/b" => FixtureAction::Respond(FixtureResponse::redirect("/target")),
        "/target" => {
            FixtureAction::Wait(Arc::clone(&handler_gate), FixtureResponse::html("landing"))
        }
        _ => FixtureAction::Respond(FixtureResponse::text(404, "missing")),
    })
    .await;
    let crawler = Crawler::new(test_client());
    let mut config = crawl_config(server.url("/"));
    config.max_pages = 3;
    config.concurrency = 2;
    let (stream, _control, stats) = crawler.stream(config).unwrap();

    server.wait_for_requests(4).await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(
        server
            .request_targets()
            .iter()
            .filter(|target| target.as_str() == "/target")
            .count(),
        1
    );
    target_gate.add_permits(1);
    let output = stream.collect::<Vec<_>>().await;

    assert_eq!(output.len(), 2);
    assert!(output.iter().all(Result::is_ok));
    assert_eq!(stats.snapshot().attempted, 3);
    assert_eq!(stats.snapshot().succeeded, 2);
    assert_eq!(stats.snapshot().skipped, 1);
}

#[tokio::test]
async fn redirect_back_to_a_completed_page_is_skipped_without_refetch() {
    let server = ControlledServer::spawn(|target| match target {
        "/" => FixtureAction::Respond(FixtureResponse::html(
            "<a href='/target'>target</a><a href='/later'>later</a>",
        )),
        "/target" => FixtureAction::Respond(FixtureResponse::html("landing")),
        "/later" => FixtureAction::Respond(FixtureResponse::redirect("/target")),
        _ => FixtureAction::Respond(FixtureResponse::text(404, "missing")),
    })
    .await;
    let crawler = Crawler::new(test_client());
    let mut config = crawl_config(server.url("/"));
    config.max_pages = 3;
    config.concurrency = 1;
    let (output, stats) = collect(&crawler, config).await;

    assert_eq!(output.len(), 2);
    assert!(output.iter().all(Result::is_ok));
    assert_eq!(
        server
            .request_targets()
            .iter()
            .filter(|target| target.as_str() == "/target")
            .count(),
        1
    );
    assert_eq!(stats.snapshot().attempted, 3);
    assert_eq!(stats.snapshot().succeeded, 2);
    assert_eq!(stats.snapshot().skipped, 1);
}

#[tokio::test]
async fn redirect_cycle_terminates_as_a_convergence_skip() {
    let server = ControlledServer::spawn(|target| match target {
        "/a" => FixtureAction::Respond(FixtureResponse::redirect("/b")),
        "/b" => FixtureAction::Respond(FixtureResponse::redirect("/a")),
        _ => FixtureAction::Respond(FixtureResponse::text(404, "missing")),
    })
    .await;
    let crawler = Crawler::new(test_client());
    let mut config = crawl_config(server.url("/a"));
    config.max_pages = 1;
    let (output, stats) = collect(&crawler, config).await;

    assert!(output.is_empty());
    assert_eq!(
        server.request_targets(),
        vec!["/a".to_owned(), "/b".to_owned()]
    );
    assert_eq!(stats.snapshot().attempted, 1);
    assert_eq!(stats.snapshot().succeeded, 0);
    assert_eq!(stats.snapshot().failed, 0);
    assert_eq!(stats.snapshot().skipped, 1);
}

#[tokio::test]
async fn failed_redirect_landing_keeps_single_lifetime_ownership() {
    let server = ControlledServer::spawn(|target| match target {
        "/" => FixtureAction::Respond(FixtureResponse::html(
            "<a href='/alias'>alias</a><a href='/target'>target</a>",
        )),
        "/alias" => FixtureAction::Respond(FixtureResponse::redirect("/target")),
        "/target" => FixtureAction::Respond(FixtureResponse::unsupported()),
        _ => FixtureAction::Respond(FixtureResponse::text(404, "missing")),
    })
    .await;
    let crawler = Crawler::new(test_client());
    let mut config = crawl_config(server.url("/"));
    config.max_pages = 3;
    config.concurrency = 2;
    let (output, stats) = collect(&crawler, config).await;

    assert_eq!(output.len(), 2);
    assert_eq!(output.iter().filter(|item| item.is_err()).count(), 1);
    assert_eq!(
        server
            .request_targets()
            .iter()
            .filter(|target| target.as_str() == "/target")
            .count(),
        1
    );
    assert_eq!(stats.snapshot().attempted, 3);
    assert_eq!(stats.snapshot().failed, 1);
    assert_eq!(stats.snapshot().skipped, 1);
}

#[tokio::test]
async fn denied_policy_reservation_does_not_shift_attempted_index_proxy_rotation() {
    let root = "http://root.invalid/".to_owned();
    let root_robots = "http://root.invalid/robots.txt".to_owned();
    let denied = "http://denied.invalid/blocked".to_owned();
    let denied_robots = "http://denied.invalid/robots.txt".to_owned();
    let allowed = "http://allowed.invalid/leaf".to_owned();
    let allowed_robots = "http://allowed.invalid/robots.txt".to_owned();

    let p0_root = root.clone();
    let p0_denied = denied.clone();
    let p0_allowed = allowed.clone();
    let p0_denied_robots = denied_robots.clone();
    let proxy_zero = ControlledServer::spawn(move |target| {
        if target.ends_with("/robots.txt") {
            let rules = if target == p0_denied_robots {
                "User-agent: rscraper\nDisallow: /blocked\n"
            } else {
                "User-agent: rscraper\nAllow: /\n"
            };
            FixtureAction::Respond(FixtureResponse::text(200, rules))
        } else if target == p0_root {
            FixtureAction::Respond(FixtureResponse::html(format!(
                "<a href='{p0_denied}'>denied</a><a href='{p0_allowed}'>allowed</a>"
            )))
        } else if target == p0_allowed {
            FixtureAction::Respond(FixtureResponse::html("allowed leaf"))
        } else {
            FixtureAction::Respond(FixtureResponse::text(500, "wrong proxy"))
        }
    })
    .await;
    let p1_root = root.clone();
    let p1_denied = denied.clone();
    let p1_allowed = allowed.clone();
    let p1_denied_robots = denied_robots.clone();
    let proxy_one = ControlledServer::spawn(move |target| {
        if target.ends_with("/robots.txt") {
            let rules = if target == p1_denied_robots {
                "User-agent: rscraper\nDisallow: /blocked\n"
            } else {
                "User-agent: rscraper\nAllow: /\n"
            };
            FixtureAction::Respond(FixtureResponse::text(200, rules))
        } else if target == p1_root {
            FixtureAction::Respond(FixtureResponse::html(format!(
                "<a href='{p1_denied}'>denied</a><a href='{p1_allowed}'>allowed</a>"
            )))
        } else if target == p1_allowed {
            FixtureAction::Respond(FixtureResponse::html("allowed leaf"))
        } else {
            FixtureAction::Respond(FixtureResponse::text(500, "wrong proxy"))
        }
    })
    .await;

    let crawler = Crawler::new(test_client());
    let mut config = crawl_config(Url::parse(&root).unwrap());
    config.max_pages = 3;
    config.concurrency = 2;
    config.same_origin_only = false;
    config.respect_robots = true;
    config.proxies = vec![proxy_zero.proxy_url(), proxy_one.proxy_url()];
    let (output, stats) = collect(&crawler, config).await;

    assert_eq!(output.len(), 2);
    assert!(output.iter().all(Result::is_ok));
    assert_eq!(stats.snapshot().attempted, 2);
    assert_eq!(stats.snapshot().succeeded, 2);
    assert_eq!(stats.snapshot().skipped, 1);
    assert_eq!(proxy_zero.request_targets(), vec![root_robots, root]);
    assert_eq!(
        proxy_one.request_targets(),
        vec![denied_robots, allowed_robots, allowed]
    );
}

#[tokio::test]
async fn failed_policy_reservation_does_not_shift_attempted_index_proxy_rotation() {
    let root = "http://root.invalid/".to_owned();
    let root_robots = "http://root.invalid/robots.txt".to_owned();
    let failed = "http://failed.invalid/unavailable".to_owned();
    let failed_robots = "http://failed.invalid/robots.txt".to_owned();
    let allowed = "http://allowed.invalid/leaf".to_owned();
    let allowed_robots = "http://allowed.invalid/robots.txt".to_owned();

    let p0_root = root.clone();
    let p0_failed = failed.clone();
    let p0_allowed = allowed.clone();
    let p0_failed_robots = failed_robots.clone();
    let proxy_zero = ControlledServer::spawn(move |target| {
        if target == p0_failed_robots {
            FixtureAction::Respond(FixtureResponse::text(503, "unavailable"))
        } else if target.ends_with("/robots.txt") {
            FixtureAction::Respond(FixtureResponse::text(
                200,
                "User-agent: rscraper\nAllow: /\n",
            ))
        } else if target == p0_root {
            FixtureAction::Respond(FixtureResponse::html(format!(
                "<a href='{p0_failed}'>failed</a><a href='{p0_allowed}'>allowed</a>"
            )))
        } else if target == p0_allowed {
            FixtureAction::Respond(FixtureResponse::html("allowed leaf"))
        } else {
            FixtureAction::Respond(FixtureResponse::text(500, "wrong proxy"))
        }
    })
    .await;
    let p1_root = root.clone();
    let p1_failed = failed.clone();
    let p1_allowed = allowed.clone();
    let p1_failed_robots = failed_robots.clone();
    let proxy_one = ControlledServer::spawn(move |target| {
        if target == p1_failed_robots {
            FixtureAction::Respond(FixtureResponse::text(503, "unavailable"))
        } else if target.ends_with("/robots.txt") {
            FixtureAction::Respond(FixtureResponse::text(
                200,
                "User-agent: rscraper\nAllow: /\n",
            ))
        } else if target == p1_root {
            FixtureAction::Respond(FixtureResponse::html(format!(
                "<a href='{p1_failed}'>failed</a><a href='{p1_allowed}'>allowed</a>"
            )))
        } else if target == p1_allowed {
            FixtureAction::Respond(FixtureResponse::html("allowed leaf"))
        } else {
            FixtureAction::Respond(FixtureResponse::text(500, "wrong proxy"))
        }
    })
    .await;

    let crawler = Crawler::new(test_client());
    let mut config = crawl_config(Url::parse(&root).unwrap());
    config.max_pages = 3;
    config.concurrency = 2;
    config.same_origin_only = false;
    config.respect_robots = true;
    config.proxies = vec![proxy_zero.proxy_url(), proxy_one.proxy_url()];
    let (output, stats) = collect(&crawler, config).await;

    assert_eq!(output.len(), 3);
    assert_eq!(output.iter().filter(|item| item.is_ok()).count(), 2);
    assert_eq!(output.iter().filter(|item| item.is_err()).count(), 1);
    assert_eq!(stats.snapshot().attempted, 2);
    assert_eq!(stats.snapshot().succeeded, 2);
    assert_eq!(stats.snapshot().failed, 0);
    assert_eq!(stats.snapshot().skipped, 1);
    assert_eq!(proxy_zero.request_targets(), vec![root_robots, root]);
    assert_eq!(
        proxy_one.request_targets(),
        vec![failed_robots, allowed_robots, allowed]
    );
}

#[tokio::test]
async fn proxy_identity_is_reserved_before_multi_origin_robots_fetches() {
    let root = "http://root.invalid/".to_owned();
    let root_robots = "http://root.invalid/robots.txt".to_owned();
    let a = "http://a.invalid/a".to_owned();
    let a_robots = "http://a.invalid/robots.txt".to_owned();
    let b = "http://b.invalid/b".to_owned();
    let b_robots = "http://b.invalid/robots.txt".to_owned();

    let p0_root = root.clone();
    let p0_a = a.clone();
    let p0_b = b.clone();
    let proxy_zero = ControlledServer::spawn(move |target| {
        if target.ends_with("/robots.txt") {
            FixtureAction::Respond(FixtureResponse::text(
                200,
                "User-agent: rscraper\nAllow: /\n",
            ))
        } else if target == p0_root {
            FixtureAction::Respond(FixtureResponse::html(format!(
                "<a href='{p0_a}'>a</a><a href='{p0_b}'>b</a>"
            )))
        } else {
            FixtureAction::Respond(FixtureResponse::html("leaf"))
        }
    })
    .await;
    let p1_root = root.clone();
    let p1_a = a.clone();
    let p1_b = b.clone();
    let proxy_one = ControlledServer::spawn(move |target| {
        if target.ends_with("/robots.txt") {
            FixtureAction::Respond(FixtureResponse::text(
                200,
                "User-agent: rscraper\nAllow: /\n",
            ))
        } else if target == p1_root {
            FixtureAction::Respond(FixtureResponse::html(format!(
                "<a href='{p1_a}'>a</a><a href='{p1_b}'>b</a>"
            )))
        } else {
            FixtureAction::Respond(FixtureResponse::html("leaf"))
        }
    })
    .await;
    let crawler = Crawler::new(test_client());
    let mut config = crawl_config(Url::parse(&root).unwrap());
    config.max_pages = 3;
    config.concurrency = 2;
    config.same_origin_only = false;
    config.respect_robots = true;
    config.proxies = vec![proxy_zero.proxy_url(), proxy_one.proxy_url()];
    let (output, stats) = collect(&crawler, config).await;

    assert_eq!(output.len(), 3);
    assert!(output.iter().all(Result::is_ok));
    assert_eq!(stats.snapshot().attempted, 3);
    assert_eq!(
        proxy_zero.request_targets(),
        vec![root_robots, root, b_robots, b]
    );
    assert_eq!(proxy_one.request_targets(), vec![a_robots, a]);
}

#[tokio::test]
async fn first_same_origin_reservation_wins_the_shared_robots_cache() {
    let root = "http://root.invalid/".to_owned();
    let same_a = "http://same.invalid/a".to_owned();
    let same_b = "http://same.invalid/b".to_owned();
    let same_robots = "http://same.invalid/robots.txt".to_owned();

    let p0_root = root.clone();
    let p0_a = same_a.clone();
    let p0_b = same_b.clone();
    let proxy_zero = ControlledServer::spawn(move |target| {
        if target.ends_with("/robots.txt") {
            FixtureAction::Respond(FixtureResponse::text(
                200,
                "User-agent: rscraper\nAllow: /\n",
            ))
        } else if target == p0_root {
            FixtureAction::Respond(FixtureResponse::html(format!(
                "<a href='{p0_a}'>a</a><a href='{p0_b}'>b</a>"
            )))
        } else {
            FixtureAction::Respond(FixtureResponse::html("leaf"))
        }
    })
    .await;
    let p1_root = root.clone();
    let p1_a = same_a.clone();
    let p1_b = same_b.clone();
    let proxy_one = ControlledServer::spawn(move |target| {
        if target.ends_with("/robots.txt") {
            FixtureAction::Respond(FixtureResponse::text(
                200,
                "User-agent: rscraper\nAllow: /\n",
            ))
        } else if target == p1_root {
            FixtureAction::Respond(FixtureResponse::html(format!(
                "<a href='{p1_a}'>a</a><a href='{p1_b}'>b</a>"
            )))
        } else {
            FixtureAction::Respond(FixtureResponse::html("leaf"))
        }
    })
    .await;
    let crawler = Crawler::new(test_client());
    let mut config = crawl_config(Url::parse(&root).unwrap());
    config.max_pages = 3;
    config.concurrency = 2;
    config.same_origin_only = false;
    config.respect_robots = true;
    config.proxies = vec![proxy_zero.proxy_url(), proxy_one.proxy_url()];
    let (output, _) = collect(&crawler, config).await;

    assert!(output.iter().all(Result::is_ok));
    assert_eq!(
        proxy_zero
            .request_targets()
            .iter()
            .filter(|target| target.as_str() == same_robots)
            .count(),
        0
    );
    assert_eq!(
        proxy_one
            .request_targets()
            .iter()
            .filter(|target| target.as_str() == same_robots)
            .count(),
        1
    );
    assert!(proxy_one.request_targets().contains(&same_a));
    assert!(proxy_zero.request_targets().contains(&same_b));
}

#[tokio::test]
async fn origin_delay_cannot_extend_or_reset_a_page_redirect_deadline() {
    let server = ControlledServer::spawn(|target| match target {
        "/start" => FixtureAction::Respond(FixtureResponse::redirect("/landing")),
        "/landing" => FixtureAction::Respond(FixtureResponse::html("too late")),
        _ => FixtureAction::Respond(FixtureResponse::text(404, "missing")),
    })
    .await;
    let crawler = Crawler::new(test_client_with_timeout(Duration::from_millis(120)));
    let mut config = crawl_config(server.url("/start"));
    config.max_pages = 1;
    config.minimum_delay = Duration::from_millis(400);
    let (output, stats) = collect(&crawler, config).await;

    assert_eq!(output.len(), 1);
    assert!(matches!(
        &output[0],
        Err(Error::Timeout {
            operation: "request"
        })
    ));
    assert_eq!(server.request_targets(), vec!["/start".to_owned()]);
    assert_eq!(stats.snapshot().failed, 1);
}

#[tokio::test]
async fn target_robots_wait_does_not_reset_the_page_redirect_deadline() {
    let destination = ControlledServer::spawn(|target| match target {
        "/robots.txt" => FixtureAction::Delay(
            Duration::from_millis(70),
            FixtureResponse::text(200, "User-agent: rscraper\nAllow: /\n"),
        ),
        "/landing" => FixtureAction::Respond(FixtureResponse::html("too late")),
        _ => FixtureAction::Respond(FixtureResponse::text(404, "missing")),
    })
    .await;
    let landing = destination.url("/landing").to_string();
    let source = ControlledServer::spawn(move |target| match target {
        "/robots.txt" => FixtureAction::Respond(FixtureResponse::text(
            200,
            "User-agent: rscraper\nAllow: /\n",
        )),
        "/start" => FixtureAction::Delay(
            Duration::from_millis(70),
            FixtureResponse::redirect(landing.clone()),
        ),
        _ => FixtureAction::Respond(FixtureResponse::text(404, "missing")),
    })
    .await;
    let crawler = Crawler::new(test_client_with_timeout(Duration::from_millis(120)));
    let mut config = crawl_config(source.url("/start"));
    config.max_pages = 1;
    config.same_origin_only = false;
    config.respect_robots = true;
    let (output, stats) = collect(&crawler, config).await;

    assert_eq!(output.len(), 1);
    assert!(matches!(
        &output[0],
        Err(Error::Timeout {
            operation: "request"
        })
    ));
    assert_eq!(
        destination.request_targets(),
        vec!["/robots.txt".to_owned()]
    );
    assert_eq!(stats.snapshot().attempted, 1);
    assert_eq!(stats.snapshot().failed, 1);
}

#[tokio::test]
async fn expired_page_does_not_emit_a_second_orphaned_robots_timeout() {
    let destination = ControlledServer::spawn(|target| match target {
        "/robots.txt" => FixtureAction::Delay(
            Duration::from_millis(300),
            FixtureResponse::text(200, "User-agent: rscraper\nAllow: /\n"),
        ),
        "/landing" => FixtureAction::Respond(FixtureResponse::html("too late")),
        _ => FixtureAction::Respond(FixtureResponse::text(404, "missing")),
    })
    .await;
    let landing = destination.url("/landing").to_string();
    let source = ControlledServer::spawn(move |target| match target {
        "/robots.txt" => FixtureAction::Respond(FixtureResponse::text(
            200,
            "User-agent: rscraper\nAllow: /\n",
        )),
        "/start" => FixtureAction::Respond(FixtureResponse::redirect(landing.clone())),
        _ => FixtureAction::Respond(FixtureResponse::text(404, "missing")),
    })
    .await;
    let crawler = Crawler::new(test_client_with_timeout(Duration::from_millis(120)));
    let mut config = crawl_config(source.url("/start"));
    config.max_pages = 1;
    config.same_origin_only = false;
    config.respect_robots = true;
    let (output, stats) = collect(&crawler, config).await;

    assert_eq!(
        output.len(),
        1,
        "one page attempt emitted multiple errors: {output:?}; stats={:?}",
        stats.snapshot()
    );
    assert!(matches!(
        &output[0],
        Err(Error::Timeout {
            operation: "request"
        })
    ));
    assert_eq!(
        destination.request_targets(),
        vec!["/robots.txt".to_owned()]
    );
    assert_eq!(stats.snapshot().failed, 1);
}

#[tokio::test]
async fn paused_expired_page_and_robots_redirects_arbitrate_one_error() {
    let robots_gate = Arc::new(Semaphore::new(0));
    let destination_gate = Arc::clone(&robots_gate);
    let destination = ControlledServer::spawn(move |target| match target {
        "/robots.txt" => FixtureAction::Wait(
            Arc::clone(&destination_gate),
            FixtureResponse::redirect("/rules"),
        ),
        "/rules" => FixtureAction::Respond(FixtureResponse::text(
            200,
            "User-agent: rscraper\nAllow: /\n",
        )),
        "/landing" => FixtureAction::Respond(FixtureResponse::html("too late")),
        _ => FixtureAction::Respond(FixtureResponse::text(404, "missing")),
    })
    .await;
    let landing = destination.url("/landing").to_string();
    let source = ControlledServer::spawn(move |target| match target {
        "/robots.txt" => FixtureAction::Respond(FixtureResponse::text(
            200,
            "User-agent: rscraper\nAllow: /\n",
        )),
        "/start" => FixtureAction::Delay(
            Duration::from_millis(40),
            FixtureResponse::redirect(landing.clone()),
        ),
        _ => FixtureAction::Respond(FixtureResponse::text(404, "missing")),
    })
    .await;
    let crawler = Crawler::new(test_client_with_timeout(Duration::from_millis(120)));
    let mut config = crawl_config(source.url("/start"));
    config.max_pages = 1;
    config.same_origin_only = false;
    config.respect_robots = true;
    let (stream, control, stats) = crawler.stream(config).unwrap();

    destination.wait_for_requests(1).await;
    control.set_paused(true).unwrap();
    robots_gate.add_permits(1);
    destination.wait_for_idle().await;
    tokio::time::sleep(Duration::from_millis(180)).await;
    control.set_paused(false).unwrap();
    let output = tokio::time::timeout(Duration::from_secs(2), stream.collect::<Vec<_>>())
        .await
        .expect("crawler did not terminate after simultaneous expiry");

    assert_eq!(
        output.len(),
        1,
        "one attempted page emitted more than once: {output:?}"
    );
    assert!(matches!(output[0], Err(Error::Timeout { .. })));
    assert_eq!(stats.snapshot().attempted, 1);
    assert_eq!(stats.snapshot().failed, 1);
    assert_eq!(destination.request_targets(), vec!["/robots.txt"]);
}

#[tokio::test]
async fn cancelling_paused_expired_page_and_robots_redirects_emits_nothing() {
    let robots_gate = Arc::new(Semaphore::new(0));
    let destination_gate = Arc::clone(&robots_gate);
    let destination = ControlledServer::spawn(move |target| match target {
        "/robots.txt" => FixtureAction::Wait(
            Arc::clone(&destination_gate),
            FixtureResponse::redirect("/rules"),
        ),
        "/rules" => FixtureAction::Respond(FixtureResponse::text(
            200,
            "User-agent: rscraper\nAllow: /\n",
        )),
        _ => FixtureAction::Respond(FixtureResponse::html("must not fetch")),
    })
    .await;
    let landing = destination.url("/landing").to_string();
    let source = ControlledServer::spawn(move |target| match target {
        "/robots.txt" => FixtureAction::Respond(FixtureResponse::text(
            200,
            "User-agent: rscraper\nAllow: /\n",
        )),
        "/start" => FixtureAction::Delay(
            Duration::from_millis(40),
            FixtureResponse::redirect(landing.clone()),
        ),
        _ => FixtureAction::Respond(FixtureResponse::text(404, "missing")),
    })
    .await;
    let crawler = Crawler::new(test_client_with_timeout(Duration::from_millis(120)));
    let mut config = crawl_config(source.url("/start"));
    config.max_pages = 1;
    config.same_origin_only = false;
    config.respect_robots = true;
    let (stream, control, _stats) = crawler.stream(config).unwrap();

    destination.wait_for_requests(1).await;
    control.set_paused(true).unwrap();
    robots_gate.add_permits(1);
    destination.wait_for_idle().await;
    tokio::time::sleep(Duration::from_millis(180)).await;
    control.cancel();
    let output = tokio::time::timeout(Duration::from_secs(2), stream.collect::<Vec<_>>())
        .await
        .expect("cancelled crawler did not terminate");

    assert!(output.is_empty());
    assert_eq!(destination.request_targets(), vec!["/robots.txt"]);
}

#[tokio::test]
async fn one_shared_origin_robots_error_is_preserved_for_multiple_live_waiters() {
    let destination = ControlledServer::spawn(|target| match target {
        "/robots.txt" => FixtureAction::Respond(FixtureResponse::text(503, "unavailable")),
        _ => FixtureAction::Respond(FixtureResponse::html("must not fetch")),
    })
    .await;
    let a = destination.url("/a");
    let b = destination.url("/b");
    let source = ControlledServer::spawn(move |target| match target {
        "/robots.txt" => FixtureAction::Respond(FixtureResponse::text(
            200,
            "User-agent: rscraper\nAllow: /\n",
        )),
        "/" => FixtureAction::Respond(FixtureResponse::html(format!(
            "<a href='{a}'>a</a><a href='{b}'>b</a>"
        ))),
        _ => FixtureAction::Respond(FixtureResponse::text(404, "missing")),
    })
    .await;
    let crawler = Crawler::new(test_client());
    let mut config = crawl_config(source.url("/"));
    config.max_pages = 3;
    config.concurrency = 3;
    config.same_origin_only = false;
    config.respect_robots = true;
    let (output, stats) = collect(&crawler, config).await;

    assert_eq!(output.len(), 2);
    assert_eq!(output.iter().filter(|item| item.is_ok()).count(), 1);
    assert_eq!(output.iter().filter(|item| item.is_err()).count(), 1);
    assert_eq!(destination.request_targets(), vec!["/robots.txt"]);
    assert_eq!(stats.snapshot().attempted, 1);
    assert_eq!(stats.snapshot().failed, 0);
    assert_eq!(stats.snapshot().skipped, 2);
}

#[tokio::test]
async fn redirect_can_claim_a_seen_but_unreserved_pending_landing() {
    let server = ControlledServer::spawn(|target| match target {
        "/" => FixtureAction::Respond(FixtureResponse::html(
            "<a href='/alias'>alias</a><a href='/target'>target</a>",
        )),
        "/alias" => FixtureAction::Respond(FixtureResponse::redirect("/target")),
        "/target" => FixtureAction::Respond(FixtureResponse::html("landing")),
        _ => FixtureAction::Respond(FixtureResponse::text(404, "missing")),
    })
    .await;
    let crawler = Crawler::new(test_client());
    let mut config = crawl_config(server.url("/"));
    config.max_pages = 2;
    config.concurrency = 1;
    let (output, stats) = collect(&crawler, config).await;

    assert_eq!(output.len(), 2);
    assert!(output.iter().all(Result::is_ok));
    assert_eq!(
        server.request_targets(),
        vec!["/".to_owned(), "/alias".to_owned(), "/target".to_owned()]
    );
    assert_eq!(stats.snapshot().attempted, 2);
    assert_eq!(stats.snapshot().succeeded, 2);
}

#[tokio::test]
async fn failed_claimed_landing_is_not_reintroduced_by_later_discovery() {
    let server = ControlledServer::spawn(|target| match target {
        "/" => FixtureAction::Respond(FixtureResponse::html(
            "<a href='/alias'>alias</a><a href='/later'>later</a>",
        )),
        "/alias" => FixtureAction::Respond(FixtureResponse::redirect("/target")),
        "/target" => FixtureAction::Respond(FixtureResponse::unsupported()),
        "/later" => {
            FixtureAction::Respond(FixtureResponse::html("<a href='/target'>do not retry</a>"))
        }
        _ => FixtureAction::Respond(FixtureResponse::text(404, "missing")),
    })
    .await;
    let crawler = Crawler::new(test_client());
    let mut config = crawl_config(server.url("/"));
    config.max_pages = 3;
    config.concurrency = 1;
    let (output, stats) = collect(&crawler, config).await;

    assert_eq!(output.len(), 3);
    assert_eq!(output.iter().filter(|item| item.is_err()).count(), 1);
    assert_eq!(
        server
            .request_targets()
            .iter()
            .filter(|target| target.as_str() == "/target")
            .count(),
        1
    );
    assert_eq!(stats.snapshot().attempted, 3);
    assert_eq!(stats.snapshot().succeeded, 2);
    assert_eq!(stats.snapshot().failed, 1);
}

#[tokio::test]
async fn cancelling_a_converged_landing_reaps_its_single_owner() {
    let target_gate = Arc::new(Semaphore::new(0));
    let handler_gate = Arc::clone(&target_gate);
    let server = ControlledServer::spawn(move |target| match target {
        "/" => FixtureAction::Respond(FixtureResponse::html(
            "<a href='/alias'>alias</a><a href='/target'>target</a>",
        )),
        "/alias" => FixtureAction::Respond(FixtureResponse::redirect("/target")),
        "/target" => {
            FixtureAction::Wait(Arc::clone(&handler_gate), FixtureResponse::html("landing"))
        }
        _ => FixtureAction::Respond(FixtureResponse::text(404, "missing")),
    })
    .await;
    let crawler = Crawler::new(test_client());
    let mut config = crawl_config(server.url("/"));
    config.max_pages = 3;
    config.concurrency = 2;
    let (stream, control, stats) = crawler.stream(config).unwrap();

    server.wait_for_requests(3).await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(
        server
            .request_targets()
            .iter()
            .filter(|target| target.as_str() == "/target")
            .count(),
        1
    );
    control.cancel();
    let output = tokio::time::timeout(Duration::from_secs(2), stream.collect::<Vec<_>>())
        .await
        .expect("cancelled converged crawl did not terminate");
    server.wait_for_idle().await;

    assert!(output.iter().all(Result::is_ok));
    assert!(output.len() <= 1);
    assert_eq!(server.active(), 0);
    assert_eq!(stats.snapshot().attempted, 3);
    assert_eq!(stats.snapshot().skipped, 1);
}
