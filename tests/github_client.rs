//! GitHub 客户端合约测试（spec 02，httpmock）

use std::time::Duration;

use hoverstare::config::{Actor, Config, PermissionKey, Permissions, PermissionsEvaluator};
use hoverstare::github::{GitHubClient, GitHubError, NewReview, Repo, RepoPermission};
use httpmock::prelude::*;

fn repo() -> Repo {
    Repo::parse("o/r").unwrap()
}

/// diff 端点 406 → 自动回退 files API，两页拼完，重组 diff 可解析
#[tokio::test]
async fn diff_406_falls_back_to_files_api() {
    let server = MockServer::start_async().await;

    let diff_mock = server
        .mock_async(|when, then| {
            when.method(GET).path("/repos/o/r/pulls/1");
            then.status(406).body("too_large");
        })
        .await;

    // 第 1 页：恰好 100 个文件（触发继续翻页）
    let page1: String = (0..100)
        .map(|i| {
            format!(
                r#"{{"filename":"src/f{i}.rs","status":"modified","patch":"@@ -1 +1 @@\n-a\n+b"}}"#
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    let files_p1 = server
        .mock_async(|when, then| {
            when.method(GET)
                .path("/repos/o/r/pulls/1/files")
                .query_param("page", "1");
            then.status(200).body(format!("[{page1}]"));
        })
        .await;
    // 第 2 页：1 个文件 + 1 个无 patch 的二进制文件
    let files_p2 = server
        .mock_async(|when, then| {
            when.method(GET)
                .path("/repos/o/r/pulls/1/files")
                .query_param("page", "2");
            then.status(200).body(
                r#"[{"filename":"src/last.rs","status":"modified","patch":"@@ -2 +2 @@\n-c\n+d"},
               {"filename":"bin/x.wasm","status":"added","patch":null}]"#,
            );
        })
        .await;

    let gh = GitHubClient::with_api_url(None, &server.base_url()).unwrap();
    let diff = gh.get_pull_request_diff(&repo(), 1).await.unwrap();

    diff_mock.assert_async().await;
    files_p1.assert_async().await;
    files_p2.assert_async().await;

    assert!(diff.contains("diff --git a/src/f0.rs b/src/f0.rs"));
    assert!(diff.contains("diff --git a/src/last.rs b/src/last.rs"));
    assert!(!diff.contains("bin/x.wasm")); // 无 patch 跳过
    // 重组结果必须能被 diff parser 解析
    let parsed = hoverstare::diff::ParsedDiff::parse(&diff);
    assert_eq!(parsed.files.len(), 101);
}

/// create_review 422 → 错误中保留完整响应体
#[tokio::test]
async fn create_review_422_surfaces_body() {
    let server = MockServer::start_async().await;
    server
        .mock_async(|when, then| {
            when.method(POST).path("/repos/o/r/pulls/1/reviews");
            then.status(422).body(
                r#"{"message":"Validation Failed","errors":["line is not part of the diff"]}"#,
            );
        })
        .await;

    let gh = GitHubClient::with_api_url(None, &server.base_url()).unwrap();
    let review = NewReview {
        commit_id: "abc".into(),
        body: "b".into(),
        comments: vec![],
    };
    let err = gh.create_review(&repo(), 1, &review).await.unwrap_err();
    match err {
        GitHubError::Api { status, body } => {
            assert_eq!(status, 422);
            assert!(body.contains("line is not part of the diff"));
        }
        other => panic!("期望 Api 错误，实际: {other}"),
    }
}

/// 429 持续限流 → 重试耗尽后 RateLimited
#[tokio::test]
async fn rate_limit_exhausts_retries() {
    let server = MockServer::start_async().await;
    let m = server
        .mock_async(|when, then| {
            when.method(GET).path("/repos/o/r/pulls/1");
            then.status(429).body("rate limited");
        })
        .await;

    let gh = GitHubClient::with_api_url(None, &server.base_url())
        .unwrap()
        .with_retry_backoff(Duration::from_millis(1));
    let err = gh.get_pull_request(&repo(), 1).await.unwrap_err();
    assert!(matches!(err, GitHubError::RateLimited));
    // 1 次首发 + 3 次重试
    m.assert_calls_async(4).await;
}

/// diff 成功路径：200 原文透传
#[tokio::test]
async fn diff_success_passthrough() {
    let server = MockServer::start_async().await;
    server
        .mock_async(|when, then| {
            when.method(GET).path("/repos/o/r/pulls/1");
            then.status(200)
                .body("diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-x\n+y\n");
        })
        .await;

    let gh = GitHubClient::with_api_url(None, &server.base_url()).unwrap();
    let diff = gh.get_pull_request_diff(&repo(), 1).await.unwrap();
    assert!(diff.contains("diff --git"));
    let parsed = hoverstare::diff::ParsedDiff::parse(&diff);
    assert_eq!(parsed.files.len(), 1);
}

// ---------------------------------------------------------------------------
// M4：GraphQL / 增量相关
// ---------------------------------------------------------------------------

/// GraphQL threads 两页分页拼接 + resolve mutation
#[tokio::test]
async fn graphql_threads_pagination_and_resolve() {
    let server = MockServer::start_async().await;

    let page1 = server.mock_async(|when, then| {
        when.method(POST)
            .path("/graphql")
            .body_includes(r#""cursor":null"#);
        then.status(200).json_body(serde_json::json!({
            "data": {"repository": {"pullRequest": {"reviewThreads": {
                "nodes": [
                    {"id": "T1", "isResolved": false, "comments": {"nodes": [{"body": "bug <!-- hoverstare-finding:aaaaaaaaaaaaaaaa -->"}]}},
                    {"id": "T2", "isResolved": true, "comments": {"nodes": [{"body": "old"}]}}
                ],
                "pageInfo": {"hasNextPage": true, "endCursor": "C1"}
            }}}}
        }));
    }).await;
    let page2 = server
        .mock_async(|when, then| {
            when.method(POST)
                .path("/graphql")
                .body_includes(r#""cursor":"C1"#);
            then.status(200).json_body(serde_json::json!({
            "data": {"repository": {"pullRequest": {"reviewThreads": {
                "nodes": [
                    {"id": "T3", "isResolved": false, "comments": {"nodes": [{"body": "another"}]}}
                ],
                "pageInfo": {"hasNextPage": false, "endCursor": null}
            }}}}
        }));
        })
        .await;
    let resolve_mock = server
        .mock_async(|when, then| {
            when.method(POST)
                .path("/graphql")
                .body_includes("resolveReviewThread")
                .body_includes("T1");
            then.status(200).json_body(serde_json::json!({
                "data": {"resolveReviewThread": {"thread": {"isResolved": true}}}
            }));
        })
        .await;

    let gh = GitHubClient::with_api_url(None, &server.base_url()).unwrap();
    let threads = gh.list_review_threads(&repo(), 1).await.unwrap();
    assert_eq!(threads.len(), 3);
    assert!(!threads[0].is_resolved);
    assert!(threads[1].is_resolved);
    page1.assert_async().await;
    page2.assert_async().await;

    gh.resolve_review_thread("T1").await.unwrap();
    resolve_mock.assert_async().await;
}

/// GraphQL HTTP 200 + errors → Graphql 错误（spec 02）
#[tokio::test]
async fn graphql_errors_field_becomes_error() {
    let server = MockServer::start_async().await;
    server
        .mock_async(|when, then| {
            when.method(POST).path("/graphql");
            then.status(200).json_body(serde_json::json!({
                "errors": [{"message": "Field 'reviewThreads' doesn't exist"}]
            }));
        })
        .await;
    let gh = GitHubClient::with_api_url(None, &server.base_url()).unwrap();
    let err = gh.list_review_threads(&repo(), 1).await.unwrap_err();
    assert!(matches!(err, GitHubError::Graphql(_)), "实际: {err}");
}

/// list_reviews + compare diff + create_status
#[tokio::test]
async fn reviews_compare_and_status() {
    let server = MockServer::start_async().await;

    server
        .mock_async(|when, then| {
            when.method(GET).path("/repos/o/r/pulls/1/reviews");
            then.status(200).json_body(serde_json::json!([
                {"id": 1, "body": "plain review", "commit_id": "aaa"},
                {"id": 2, "body": "<!-- hoverstare-meta\nhead_sha: bbb123\n-->", "commit_id": "bbb123"}
            ]));
        })
        .await;
    server
        .mock_async(|when, then| {
            when.method(GET).path("/repos/o/r/compare/bbb123...ccc456");
            then.status(200)
                .body("diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-x\n+y\n");
        })
        .await;
    let status_mock = server
        .mock_async(|when, then| {
            when.method(POST)
                .path("/repos/o/r/statuses/ccc456")
                .body_includes(r#""context":"hoverstare-findings""#)
                .body_includes(r#""state":"failure""#);
            then.status(200).json_body(serde_json::json!({"id": 1}));
        })
        .await;

    let gh = GitHubClient::with_api_url(None, &server.base_url()).unwrap();

    let reviews = gh.list_reviews(&repo(), 1).await.unwrap();
    assert_eq!(reviews.len(), 2);
    let prior = hoverstare::state::parse_meta_head_sha(&reviews[1].body);
    assert_eq!(prior.as_deref(), Some("bbb123"));

    let diff = gh
        .get_compare_diff(&repo(), "bbb123", "ccc456")
        .await
        .unwrap();
    assert!(diff.contains("diff --git"));

    gh.create_status(
        &repo(),
        "ccc456",
        &hoverstare::github::NewStatus {
            context: "hoverstare-findings",
            state: hoverstare::github::StatusState::Failure,
            description: "存在高危".into(),
        },
    )
    .await
    .unwrap();
    status_mock.assert_async().await;
}

// ---------------------------------------------------------------------------
// M6：mention 命令相关
// ---------------------------------------------------------------------------

/// issue 评论与 review 线程评论的 reaction 端点不同（spec 09）
#[tokio::test]
async fn reactions_use_correct_endpoints() {
    let server = MockServer::start_async().await;

    let issue_reaction = server
        .mock_async(|when, then| {
            when.method(POST)
                .path("/repos/o/r/issues/comments/11/reactions")
                .body_includes(r#""content":"rocket""#);
            then.status(200).json_body(serde_json::json!({"id": 1}));
        })
        .await;
    let review_reaction = server
        .mock_async(|when, then| {
            when.method(POST)
                .path("/repos/o/r/pulls/comments/22/reactions")
                .body_includes(r#""content":"+1""#);
            then.status(200).json_body(serde_json::json!({"id": 2}));
        })
        .await;

    let gh = GitHubClient::with_api_url(None, &server.base_url()).unwrap();

    // issue_comment（无线程）
    let ev_issue = hoverstare::event::MentionEvent {
        repo: "o/r".into(),
        pr_number: 1,
        comment_id: 11,
        body: "@hoverstare review".into(),
        author_association: "OWNER".into(),
        author: "u".into(),
        user_type: "User".into(),
        in_reply_to: None,
    };
    gh.create_reaction(&repo(), &ev_issue, "rocket")
        .await
        .unwrap();
    issue_reaction.assert_async().await;

    // pull_request_review_comment（线程回复）
    let ev_review = hoverstare::event::MentionEvent {
        comment_id: 22,
        in_reply_to: Some(9),
        ..ev_issue.clone()
    };
    gh.create_reaction(&repo(), &ev_review, "+1").await.unwrap();
    review_reaction.assert_async().await;
}

/// 取线程评论正文（explain 上下文）
#[tokio::test]
async fn fetch_review_comment_body() {
    let server = MockServer::start_async().await;
    server
        .mock_async(|when, then| {
            when.method(GET).path("/repos/o/r/pulls/comments/9");
            then.status(200)
                .json_body(serde_json::json!({"id": 9, "body": "🟠 **HIGH**: 空指针"}));
        })
        .await;
    let gh = GitHubClient::with_api_url(None, &server.base_url()).unwrap();
    let body = gh.get_review_comment_body(&repo(), 9).await.unwrap();
    assert!(body.contains("空指针"));
}

/// 取线程评论的锚定信息 path/diff_hunk（finding 线程讨论上下文，issue #13）
#[tokio::test]
async fn fetch_review_comment_detail() {
    let server = MockServer::start_async().await;
    let m = server
        .mock_async(|when, then| {
            when.method(GET).path("/repos/o/r/pulls/comments/9");
            then.status(200).json_body(serde_json::json!({
                "id": 9,
                "body": "🟠 **HIGH**: 空指针 <!-- hoverstare-finding:0123456789abcdef -->",
                "path": "src/a.rs",
                "diff_hunk": "@@ -1,2 +1,2 @@\n-let x = 1;\n+let x = 2;",
            }));
        })
        .await;
    let gh = GitHubClient::with_api_url(None, &server.base_url()).unwrap();
    let detail = gh.get_review_comment(&repo(), 9).await.unwrap();
    assert!(detail.body.contains("hoverstare-finding"));
    assert_eq!(detail.path, "src/a.rs");
    assert!(detail.diff_hunk.contains("+let x = 2;"));
    m.assert_async().await;
}

/// 线程历史拉取：分页 + 只保留本线程评论 + 按 id 时间序（issue #13 多轮记忆）
#[tokio::test]
async fn list_review_thread_comments_filters_and_paginates() {
    let server = MockServer::start_async().await;

    // 第 1 页：恰好 100 条（触发继续翻页），其中 2 条属于线程 9
    let page1: String = (0..100)
        .map(|i| {
            if i == 0 {
                r#"{"id":9,"body":"finding","user":{"login":"hoverstare[bot]"},"in_reply_to_id":null}"#.to_string()
            } else if i == 1 {
                r#"{"id":10,"body":"first question","user":{"login":"alice"},"in_reply_to_id":9}"#.to_string()
            } else {
                format!(
                    r#"{{"id":{},"body":"other thread","user":{{"login":"bob"}},"in_reply_to_id":{}}}"#,
                    1000 + i,
                    555
                )
            }
        })
        .collect::<Vec<_>>()
        .join(",");
    let p1 = server
        .mock_async(|when, then| {
            when.method(GET)
                .path("/repos/o/r/pulls/1/comments")
                .query_param("page", "1");
            then.status(200).body(format!("[{page1}]"));
        })
        .await;
    let p2 = server
        .mock_async(|when, then| {
            when.method(GET)
                .path("/repos/o/r/pulls/1/comments")
                .query_param("page", "2");
            then.status(200).json_body(serde_json::json!([
                {"id": 12, "body": "bot answer", "user": {"login": "hoverstare[bot]"}, "in_reply_to_id": 9},
                {"id": 11, "body": "follow up", "user": {"login": "alice"}, "in_reply_to_id": 9}
            ]));
        })
        .await;

    let gh = GitHubClient::with_api_url(None, &server.base_url()).unwrap();
    let comments = gh.list_review_thread_comments(&repo(), 1, 9).await.unwrap();
    p1.assert_async().await;
    p2.assert_async().await;

    // 只保留线程 9 的评论，且按 id 时间序（第 2 页的 11/12 被重新排序）
    let ids: Vec<u64> = comments.iter().map(|c| c.id).collect();
    assert_eq!(ids, vec![9, 10, 11, 12]);
    assert_eq!(comments[1].author, "alice");
    assert_eq!(comments[3].body, "bot answer");
}

/// mention 事件解析：issue_comment（PR）/ 纯 issue / review 线程回复
#[test]
fn mention_event_parsing() {
    let dir = tempfile::tempdir().unwrap();
    let write = |name: &str, content: &str| {
        let p = dir.path().join(name);
        std::fs::write(&p, content).unwrap();
        p
    };
    unsafe {
        std::env::set_var("GITHUB_REPOSITORY", "o/r");
    }

    // issue_comment on PR
    let p = write(
        "e1.json",
        r#"{"issue": {"number": 5, "pull_request": {"url": "..."}},
            "comment": {"id": 11, "body": "@hoverstare review", "author_association": "OWNER", "in_reply_to_id": null}}"#,
    );
    unsafe {
        std::env::set_var("GITHUB_EVENT_PATH", &p);
    }
    let ev = hoverstare::event::resolve_mention().unwrap().unwrap();
    assert_eq!(ev.pr_number, 5);
    assert_eq!(ev.comment_id, 11);
    assert!(ev.is_collaborator());
    assert_eq!(ev.in_reply_to_id(), None);

    // 纯 issue（无 pull_request 字段）→ None
    let p = write(
        "e2.json",
        r#"{"issue": {"number": 6},
            "comment": {"id": 12, "body": "@hoverstare review", "author_association": "OWNER", "in_reply_to_id": null}}"#,
    );
    unsafe {
        std::env::set_var("GITHUB_EVENT_PATH", &p);
    }
    assert!(hoverstare::event::resolve_mention().unwrap().is_none());

    // pull_request_review_comment（线程回复）
    let p = write(
        "e3.json",
        r#"{"pull_request": {"number": 7},
            "comment": {"id": 22, "body": "@hoverstare explain", "author_association": "NONE", "in_reply_to_id": 9}}"#,
    );
    unsafe {
        std::env::set_var("GITHUB_EVENT_PATH", &p);
    }
    let ev = hoverstare::event::resolve_mention().unwrap().unwrap();
    assert_eq!(ev.pr_number, 7);
    assert_eq!(ev.in_reply_to_id(), Some(9));
    assert!(!ev.is_collaborator()); // NONE → 无权限
}

/// resolve 降级路径：REST 线程回复端点（spec 07）
#[tokio::test]
async fn reply_to_review_comment_works() {
    let server = MockServer::start_async().await;
    let m = server
        .mock_async(|when, then| {
            when.method(POST)
                .path("/repos/o/r/pulls/1/comments/9/replies")
                .body_includes("已确认修复");
            then.status(200).json_body(serde_json::json!({"id": 10}));
        })
        .await;
    let gh = GitHubClient::with_api_url(None, &server.base_url()).unwrap();
    gh.reply_to_review_comment(&repo(), 1, 9, "✅ HoverStare 已确认修复")
        .await
        .unwrap();
    m.assert_async().await;
}

// ---------------------------------------------------------------------------
// 终态 status check（spec 07：跳过路径也必须落地，否则 required check 死锁）
// ---------------------------------------------------------------------------

/// env 依赖测试的串行锁（GITHUB_API_URL 是进程级共享状态）
static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn cfg_with_status_checks(status_checks: bool) -> hoverstare::config::Config {
    unsafe {
        std::env::set_var("OPENAI_API_KEY", "test");
    }
    let mut c = hoverstare::config::Config::load().unwrap();
    c.status_checks = status_checks;
    c
}

#[tokio::test]
async fn skipped_run_still_posts_status_check() {
    let _guard = ENV_LOCK.lock().await;
    let server = MockServer::start_async().await;
    server
        .mock_async(|when, then| {
            when.method(GET).path("/repos/o/r/pulls/1");
            then.status(200).json_body(serde_json::json!({
                "number": 1,
                "head": {"sha": "abc123", "ref": "feat"},
                "base": {"sha": "def456", "ref": "main"},
                "draft": true,
                "user": {"login": "dev"}
            }));
        })
        .await;
    let status_mock = server
        .mock_async(|when, then| {
            when.method(POST)
                .path("/repos/o/r/statuses/abc123")
                .body_includes(r#""context":"hoverstare""#)
                .body_includes(r#""state":"success""#)
                .body_includes("Skipped");
            then.status(200).json_body(serde_json::json!({"id": 1}));
        })
        .await;

    unsafe {
        std::env::set_var("GITHUB_API_URL", server.base_url());
    }
    let outcome = hoverstare::orchestrator::run_review(
        &cfg_with_status_checks(true),
        &hoverstare::cli::ReviewArgs {
            pr: Some(1),
            repo: Some("o/r".into()),
            ..Default::default()
        },
        false,
    )
    .await
    .unwrap();
    assert!(matches!(
        outcome,
        hoverstare::orchestrator::Outcome::Skipped(_)
    ));
    status_mock.assert_async().await;
}

#[tokio::test]
async fn no_status_check_when_disabled() {
    let _guard = ENV_LOCK.lock().await;
    let server = MockServer::start_async().await;
    server
        .mock_async(|when, then| {
            when.method(GET).path("/repos/o/r/pulls/1");
            then.status(200).json_body(serde_json::json!({
                "number": 1,
                "head": {"sha": "abc123", "ref": "feat"},
                "base": {"sha": "def456", "ref": "main"},
                "draft": true,
                "user": {"login": "dev"}
            }));
        })
        .await;
    // 不写 status 的 mock：任何 statuses 调用都视为意外
    let status_mock = server
        .mock_async(|when, then| {
            when.method(POST).path("/repos/o/r/statuses/abc123");
            then.status(500);
        })
        .await;

    unsafe {
        std::env::set_var("GITHUB_API_URL", server.base_url());
    }
    let outcome = hoverstare::orchestrator::run_review(
        &cfg_with_status_checks(false),
        &hoverstare::cli::ReviewArgs {
            pr: Some(1),
            repo: Some("o/r".into()),
            ..Default::default()
        },
        false,
    )
    .await
    .unwrap();
    assert!(matches!(
        outcome,
        hoverstare::orchestrator::Outcome::Skipped(_)
    ));
    status_mock.assert_calls_async(0).await;
}

// ---------------------------------------------------------------------------
// M14：细粒度权限系统（spec 12）
// ---------------------------------------------------------------------------

/// merge 命令权限不足：返回说明 + 👀 reaction，不调用后续流程（无 LLM / 无合并）
#[tokio::test]
async fn merge_permission_denied_reacts_and_skips() {
    let _guard = ENV_LOCK.lock().await;
    let server = MockServer::start_async().await;

    // 协作者权限 API 返回 triage（低于默认 merge 要求的 write）
    server
        .mock_async(|when, then| {
            when.method(GET)
                .path("/repos/o/r/collaborators/dev/permission");
            then.status(200)
                .json_body(serde_json::json!({"permission": "triage"}));
        })
        .await;

    let comment_mock = server
        .mock_async(|when, then| {
            when.method(POST)
                .path("/repos/o/r/issues/1/comments")
                .body_includes("Permission denied");
            then.status(200).json_body(serde_json::json!({"id": 99}));
        })
        .await;
    let reaction_mock = server
        .mock_async(|when, then| {
            when.method(POST)
                .path("/repos/o/r/issues/comments/11/reactions")
                .body_includes(r#""content":"eyes""#);
            then.status(200).json_body(serde_json::json!({"id": 100}));
        })
        .await;

    unsafe {
        std::env::set_var("GITHUB_API_URL", server.base_url());
        std::env::set_var("OPENAI_API_KEY", "test");
    }

    let cfg = Config::load().unwrap();
    let ev = hoverstare::event::DevEvent {
        repo: "o/r".into(),
        number: 1,
        is_pr: true,
        kind: hoverstare::event::DevKind::IssueComment,
        title: None,
        body: "@hoverstare merge".into(),
        comment_id: Some(11),
        author_association: "MEMBER".into(),
        in_reply_to: None,
        author: "dev".into(),
    };

    let result = hoverstare::devagent::run_event(&cfg, &ev).await.unwrap();
    assert!(result.contains("permission denied"), "actual: {result}");

    comment_mock.assert_async().await;
    reaction_mock.assert_async().await;
}

// ---------------------------------------------------------------------------
// M14：细粒度权限系统补充合约测试（spec 12）
// ---------------------------------------------------------------------------

/// get_collaborator_permission 解析五种权限等级（spec 12）。
#[tokio::test]
async fn get_collaborator_permission_parses_levels() {
    for (name, expected) in [
        ("read", RepoPermission::Read),
        ("triage", RepoPermission::Triage),
        ("write", RepoPermission::Write),
        ("maintain", RepoPermission::Maintain),
        ("admin", RepoPermission::Admin),
    ] {
        let server = MockServer::start_async().await;
        server
            .mock_async(|when, then| {
                when.method(GET)
                    .path("/repos/o/r/collaborators/dev/permission");
                then.status(200)
                    .json_body(serde_json::json!({"permission": name}));
            })
            .await;

        let gh = GitHubClient::with_api_url(None, &server.base_url()).unwrap();
        let got = gh
            .get_collaborator_permission(&repo(), "dev")
            .await
            .unwrap();
        assert_eq!(got, expected, "unexpected level for {name}");
    }
}

/// 协作者权限等级：API 返回等级 >= 配置等级时通过。
#[tokio::test]
async fn permission_level_greater_or_equal() {
    let server = MockServer::start_async().await;
    server
        .mock_async(|when, then| {
            when.method(GET)
                .path("/repos/o/r/collaborators/dev/permission");
            then.status(200)
                .json_body(serde_json::json!({"permission": "maintain"}));
        })
        .await;

    let gh = GitHubClient::with_api_url(None, &server.base_url()).unwrap();
    let perms = Permissions {
        develop: vec!["read".into()],
        merge: vec!["write".into()],
        ..Permissions::default()
    };
    let evaluator = PermissionsEvaluator::new(perms);
    let actor = Actor {
        login: "dev",
        author_association: "NONE",
    };

    assert!(
        evaluator
            .evaluate(PermissionKey::Develop, &gh, &repo(), actor)
            .await
    );
    assert!(
        evaluator
            .evaluate(PermissionKey::Merge, &gh, &repo(), actor)
            .await
    );
}

/// 协作者权限等级不足时应拒绝。
#[tokio::test]
async fn permission_level_too_low_is_denied() {
    let server = MockServer::start_async().await;
    server
        .mock_async(|when, then| {
            when.method(GET)
                .path("/repos/o/r/collaborators/dev/permission");
            then.status(200)
                .json_body(serde_json::json!({"permission": "triage"}));
        })
        .await;

    let gh = GitHubClient::with_api_url(None, &server.base_url()).unwrap();
    let perms = Permissions {
        merge: vec!["write".into()],
        ..Permissions::default()
    };
    let evaluator = PermissionsEvaluator::new(perms);
    let actor = Actor {
        login: "dev",
        author_association: "COLLABORATOR",
    };

    assert!(
        !evaluator
            .evaluate(PermissionKey::Merge, &gh, &repo(), actor)
            .await
    );
}

/// 同一用户跨多个命令键时，协作者权限 API 只调用一次（spec 12 §5 缓存）。
#[tokio::test]
async fn permission_level_cache_per_user() {
    let server = MockServer::start_async().await;
    let m = server
        .mock_async(|when, then| {
            when.method(GET)
                .path("/repos/o/r/collaborators/dev/permission");
            then.status(200)
                .json_body(serde_json::json!({"permission": "admin"}));
        })
        .await;

    let gh = GitHubClient::with_api_url(None, &server.base_url()).unwrap();
    let perms = Permissions {
        develop: vec!["read".into()],
        merge: vec!["write".into()],
        ..Permissions::default()
    };
    let evaluator = PermissionsEvaluator::new(perms);
    let actor = Actor {
        login: "dev",
        author_association: "NONE",
    };

    assert!(
        evaluator
            .evaluate(PermissionKey::Develop, &gh, &repo(), actor)
            .await
    );
    assert!(
        evaluator
            .evaluate(PermissionKey::Merge, &gh, &repo(), actor)
            .await
    );
    m.assert_calls_async(1).await;
}

/// @org/team 命中：用户在团队中且状态 active。
#[tokio::test]
async fn team_membership_hit() {
    let server = MockServer::start_async().await;
    server
        .mock_async(|when, then| {
            when.method(GET).path("/orgs/o/teams/t/memberships/dev");
            then.status(200)
                .json_body(serde_json::json!({"state": "active"}));
        })
        .await;

    let gh = GitHubClient::with_api_url(None, &server.base_url()).unwrap();
    let perms = Permissions {
        develop: vec!["@o/t".into()],
        ..Permissions::default()
    };
    let evaluator = PermissionsEvaluator::new(perms);
    let actor = Actor {
        login: "dev",
        author_association: "NONE",
    };

    assert!(
        evaluator
            .evaluate(PermissionKey::Develop, &gh, &repo(), actor)
            .await
    );
}

/// @org/team 未命中：404 视为非成员（个人仓库或团队不存在）。
#[tokio::test]
async fn team_membership_miss() {
    let server = MockServer::start_async().await;
    server
        .mock_async(|when, then| {
            when.method(GET).path("/orgs/o/teams/t/memberships/dev");
            then.status(404);
        })
        .await;

    let gh = GitHubClient::with_api_url(None, &server.base_url()).unwrap();
    let perms = Permissions {
        develop: vec!["@o/t".into()],
        ..Permissions::default()
    };
    let evaluator = PermissionsEvaluator::new(perms);
    let actor = Actor {
        login: "dev",
        author_association: "OWNER",
    };

    assert!(
        !evaluator
            .evaluate(PermissionKey::Develop, &gh, &repo(), actor)
            .await
    );
}

/// T17.9（spec 14 §4）：分析区失败时覆盖声明必须可见——失败不再只说"失败"。
///
/// 覆盖计数落在状态检查描述里（fail-open 下不会有评论，状态检查是唯一载体），
/// 因此这条测试同时锁住三件事：分析失败仍然 fail-open（exit 0 语义）、
/// 分母在派发前冻结（1 个单元）、失败注记带 `coverage 0/1` 而不是一句空泛的失败。
#[tokio::test]
async fn analysis_failure_status_check_states_coverage() {
    let _guard = ENV_LOCK.lock().await;
    let server = MockServer::start_async().await;

    // diff 请求（Accept: v3.diff）——更具体的 mock 先注册，JSON 请求会落到下一个
    server
        .mock_async(|when, then| {
            when.method(GET)
                .path("/repos/o/r/pulls/1")
                .header("accept", "application/vnd.github.v3.diff");
            then.status(200)
                .body("diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-x\n+y\n");
        })
        .await;

    server
        .mock_async(|when, then| {
            when.method(GET).path("/repos/o/r/pulls/1");
            then.status(200).json_body(serde_json::json!({
                "number": 1,
                "head": {"sha": "abc123", "ref": "feat"},
                "base": {"sha": "def456", "ref": "main"},
                "draft": false,
                "user": {"login": "dev"},
                "author_association": "OWNER"
            }));
        })
        .await;

    // 历史 review 列表为空 → 全量模式
    server
        .mock_async(|when, then| {
            when.method(GET).path("/repos/o/r/pulls/1/reviews");
            then.status(200).json_body(serde_json::json!([]));
        })
        .await;

    let status_mock = server
        .mock_async(|when, then| {
            when.method(POST)
                .path("/repos/o/r/statuses/abc123")
                .body_includes(r#""context":"hoverstare""#)
                .body_includes("coverage 0/1")
                .body_includes("analysis failed (fail-open)");
            then.status(200).json_body(serde_json::json!({"id": 1}));
        })
        .await;

    unsafe {
        std::env::set_var("GITHUB_API_URL", server.base_url());
        // 模型端点指向没有监听者的端口：每个 pass 必然失败，且不产生真实调用
        std::env::set_var("OPENAI_BASE_URL", "http://127.0.0.1:1/v1");
    }

    let outcome = hoverstare::orchestrator::run_review(
        &cfg_with_status_checks(true),
        &hoverstare::cli::ReviewArgs {
            pr: Some(1),
            repo: Some("o/r".into()),
            ..Default::default()
        },
        false,
    )
    .await
    .unwrap();

    assert!(
        matches!(
            outcome,
            hoverstare::orchestrator::Outcome::AnalysisFailed(_)
        ),
        "analysis failure must stay fail-open"
    );
    status_mock.assert_async().await;

    // Leave the environment as we found it: this test is the only one that
    // redirects the model endpoint.
    unsafe {
        std::env::remove_var("OPENAI_BASE_URL");
    }
}

/// M19 验收（spec 16 §2）：一次成功运行必须能产出**契约文档**——
/// JSON 直出 stdout 或按 `--output` 落到工作区内，字段/枚举/排序由契约固定。
///
/// 台架要点：模型端点被 mock 成"每次都返回同一个 finding"，diff 有 60 个新增行
/// （因此走 3 路 pass，两票以上直接入选，不需要 verifier），GitHub 侧只 mock
/// PR 元数据 / diff / review 列表 / 发布 review。JSON 文档落在工作区内（GITHUB_WORKSPACE
/// 指向临时目录），既能断言内容，也顺带验证 `--output` 的沙箱落点。
#[tokio::test]
async fn run_review_emits_the_json_contract() {
    let _guard = ENV_LOCK.lock().await;
    let server = MockServer::start_async().await;
    let provider = MockServer::start_async().await;
    let workspace = tempfile::tempdir().unwrap();

    // diff（Accept: v3.diff）：60 个新增行 → 非小 diff → 3 路 pass
    let mut diff = String::from(
        "diff --git a/src/a.rs b/src/a.rs\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1,0 +1,60 @@\n",
    );
    for i in 1..=60 {
        diff.push_str(&format!("+line {i}\n"));
    }
    server
        .mock_async(|when, then| {
            when.method(GET)
                .path("/repos/o/r/pulls/1")
                .header("accept", "application/vnd.github.v3.diff");
            then.status(200).body(diff.clone());
        })
        .await;

    server
        .mock_async(|when, then| {
            when.method(GET).path("/repos/o/r/pulls/1");
            then.status(200).json_body(serde_json::json!({
                "number": 1,
                "head": {"sha": "abc123", "ref": "feat"},
                "base": {"sha": "def456", "ref": "main"},
                "draft": false,
                "user": {"login": "dev"},
                "author_association": "OWNER"
            }));
        })
        .await;

    server
        .mock_async(|when, then| {
            when.method(GET).path("/repos/o/r/pulls/1/reviews");
            then.status(200).json_body(serde_json::json!([]));
        })
        .await;

    let review_mock = server
        .mock_async(|when, then| {
            when.method(POST).path("/repos/o/r/pulls/1/reviews");
            then.status(201).json_body(serde_json::json!({"id": 1}));
        })
        .await;

    // 模型：每次调用都返回同一个 finding（3 路一致 → 两票以上直接入选）
    let completion = serde_json::json!({
        "id": "1",
        "object": "chat.completion",
        "created": 1,
        "model": "m",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": r#"{"findings":[{"file":"src/a.rs","line":5,"severity":"high","title":"boom","description":"d"}],"summary":"s"}"#
            },
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 100, "completion_tokens": 10, "total_tokens": 110}
    });
    provider
        .mock_async(|when, then| {
            when.method(POST).path("/v1/chat/completions");
            then.status(200).json_body(completion.clone());
        })
        .await;

    unsafe {
        std::env::set_var("GITHUB_API_URL", server.base_url());
        std::env::set_var("OPENAI_BASE_URL", format!("{}/v1", provider.base_url()));
        std::env::set_var("GITHUB_WORKSPACE", workspace.path());
    }

    let cfg = cfg_with_status_checks(false);
    let configured_model = cfg.model.clone();
    let outcome = hoverstare::orchestrator::run_review(
        &cfg,
        &hoverstare::cli::ReviewArgs {
            pr: Some(1),
            repo: Some("o/r".into()),
            format: hoverstare::output::OutputFormat::Json,
            output: Some("out/result.json".to_string()),
            ..Default::default()
        },
        false,
    )
    .await
    .unwrap();

    assert!(
        matches!(
            outcome,
            hoverstare::orchestrator::Outcome::Published { terminal, .. }
                if terminal == hoverstare::units::TerminalState::Ok
        ),
        "a successful run must report ok coverage, got {outcome:?}"
    );
    review_mock.assert_async().await;

    let written = std::fs::read_to_string(workspace.path().join("out/result.json"))
        .expect("the JSON document must be written to the requested path");
    let doc: serde_json::Value = serde_json::from_str(&written).unwrap();

    assert_eq!(doc["schema_version"], "1.0");
    assert_eq!(doc["run"]["repository"], "o/r");
    assert_eq!(doc["run"]["change_request"], 1);
    assert_eq!(doc["run"]["terminal"], "ok");
    assert_eq!(
        doc["run"]["usage"]["calls"], 3,
        "three passes were paid for"
    );
    assert_eq!(doc["run"]["usage"]["input_tokens"], 300);
    assert_eq!(doc["run"]["model"], configured_model);
    assert!(doc["run"]["timing"]["duration_ms"].is_u64());
    assert_eq!(doc["findings"][0]["path"], "src/a.rs");
    assert_eq!(doc["findings"][0]["line"], 5);
    assert_eq!(doc["findings"][0]["side"], "new");
    assert_eq!(doc["findings"][0]["severity"], "high");
    assert_eq!(doc["findings"][0]["status"], "new");
    assert_eq!(doc["units"][0]["status"], "covered");
    assert_eq!(doc["coverage"]["covered"], 1);
    assert!(doc["resolutions"].as_array().unwrap().is_empty());

    unsafe {
        std::env::remove_var("OPENAI_BASE_URL");
        std::env::remove_var("GITHUB_WORKSPACE");
    }
}
