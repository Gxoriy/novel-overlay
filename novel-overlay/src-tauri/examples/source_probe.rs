//! 书源实测探针：用生产采集链路（同一 UA、robots 闸门、限速、挑战检测）
//! 跑「搜索 → 目录 → 正文」三步，三步都过才算可用。
//!
//! ```bash
//! cargo run --example source_probe -- --json=../sources/candidates.json --keyword=斗破苍穹
//! cargo run --example source_probe -- --json=../sources/candidates.json --import --db=../docs/novel-overlay.db
//! ```

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use novel_overlay_lib::engine::{JsEvaluator, PassThroughJs, QuickJsEvaluator, SourceEngine};
use novel_overlay_lib::model::BookSource;
use novel_overlay_lib::net::{
    clock, discover_local_proxies, Fetcher, Ladder, PageSource, ProxyPool, ProxySpec,
    PublicWebConfig, PublicWebTier, Transport,
};

struct Args {
    json: PathBuf,
    keyword: String,
    import: bool,
    db: Option<PathBuf>,
}

fn parse_args() -> Result<Args> {
    let mut json = None;
    let mut keyword = "斗破苍穹".to_string();
    let mut import = false;
    let mut db = None;

    for arg in std::env::args().skip(1) {
        if let Some(rest) = arg.strip_prefix("--json=") {
            json = Some(PathBuf::from(rest));
        } else if let Some(rest) = arg.strip_prefix("--keyword=") {
            keyword = rest.to_string();
        } else if let Some(rest) = arg.strip_prefix("--db=") {
            db = Some(PathBuf::from(rest));
        } else if arg == "--import" {
            import = true;
        }
    }

    Ok(Args {
        json: json.context("缺少 --json=<书源文件>")?,
        keyword,
        import,
        db,
    })
}

fn build_ladder() -> Result<Arc<Ladder>> {
    let clock = clock::system();
    let proxies = ProxyPool::from_env(clock.clone()).map(Arc::new);
    if let Some(pool) = &proxies {
        println!("代理池: {}", pool.describe());
    } else {
        let local = discover_local_proxies();
        if local.is_empty() {
            println!("代理池: 未配置，也未探测到本机代理");
        } else {
            println!(
                "代理池: 自动使用 {}",
                local
                    .iter()
                    .map(ProxySpec::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }

    let mut fetcher = Fetcher::with_clock(1.0, 2, clock.clone())?;
    if let Some(pool) = proxies {
        fetcher = fetcher.with_proxies(pool);
    }

    let transport: Arc<dyn Transport> = Arc::new(fetcher);
    let config = PublicWebConfig::new().enabled().breaker(15, 120.0);
    let public_web = PublicWebTier::new(transport, clock, config);
    Ok(Arc::new(Ladder::new(vec![Box::new(public_web)])))
}

async fn build_js() -> Box<dyn JsEvaluator> {
    match QuickJsEvaluator::spawn() {
        Ok(evaluator) => Box::new(evaluator),
        Err(error) => {
            println!("QuickJS 不可用（js 规则会直通）: {error}");
            Box::new(PassThroughJs)
        }
    }
}

fn load_sources(path: &PathBuf) -> Result<Vec<BookSource>> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("读不到书源文件 {}", path.display()))?;
    let value: serde_json::Value = serde_json::from_str(&raw).context("书源文件不是合法 JSON")?;
    match value {
        serde_json::Value::Array(items) => items
            .into_iter()
            .map(|item| serde_json::from_value(item).context("书源字段不符合 schema"))
            .collect(),
        other => Ok(vec![
            serde_json::from_value(other).context("书源字段不符合 schema")?
        ]),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = parse_args()?;
    let sources = load_sources(&args.json)?;
    let ladder = build_ladder()?;
    let js = build_js().await;
    let engine = SourceEngine::new(ladder.as_ref(), js.as_ref());

    let mut passed: Vec<BookSource> = Vec::new();
    for (index, source) in sources.iter().enumerate() {
        println!(
            "\n===== [{}] {} ({})",
            index + 1,
            source.book_source_name,
            source.book_source_url
        );

        let step = Instant::now();
        let books = match engine.search(source, &args.keyword).await {
            Ok(books) => books,
            Err(error) => {
                println!("  搜索失败: {error:#}");
                continue;
            }
        };
        println!("  搜索: {} 条（{:.1}s）", books.len(), step.elapsed().as_secs_f32());
        for book in books.iter().take(3) {
            println!(
                "    - {} / {} / {}",
                book.name,
                if book.author.is_empty() { "?" } else { &book.author },
                book.book_url
            );
        }
        let Some(first) = books.first() else {
            // 0 条时把引擎实际拿到的搜索页存下来，方便对照选择器。
            let template = source.search_url.replace("{{key}}", &args.keyword);
            let url = url::Url::parse(&source.book_source_url)
                .and_then(|base| base.join(&template))
                .map(|u| u.to_string())
                .unwrap_or_else(|_| template.clone());
            let headers = source.headers().unwrap_or_default();
            match ladder
                .get_text(&url, &headers, source.challenge_markers())
                .await
            {
                Ok(html) => {
                    let dir = std::path::Path::new("../probe");
                    std::fs::create_dir_all(dir).ok();
                    let path = dir.join(format!("engine-search-{index}.html"));
                    match std::fs::write(&path, &html) {
                        Ok(()) => println!(
                            "  已保存搜索页（{} 字节）→ {}",
                            html.len(),
                            path.display()
                        ),
                        Err(error) => println!("  保存搜索页失败: {error}"),
                    }
                }
                Err(error) => println!("  搜索页也取不到: {error:#}"),
            }
            println!("  ✗ 无结果，判定不可用");
            continue;
        };

        let step = Instant::now();
        let chapters = match engine.toc(source, &first.book_url).await {
            Ok(chapters) => chapters,
            Err(error) => {
                println!("  目录失败: {error:#}");
                continue;
            }
        };
        println!(
            "  目录: {} 章（{:.1}s）",
            chapters.len(),
            step.elapsed().as_secs_f32()
        );
        for chapter in chapters.iter().take(3) {
            println!("    - {} {} ({})", chapter.index, chapter.name, chapter.url);
        }
        let Some(chapter) = chapters.first() else {
            println!("  ✗ 目录为空，判定不可用");
            continue;
        };

        let step = Instant::now();
        let content = match engine.content(source, &chapter.url).await {
            Ok(content) => content,
            Err(error) => {
                println!("  正文失败: {error:#}");
                continue;
            }
        };
        let chars = content.chars().count();
        let head: String = content.chars().take(40).collect();
        println!(
            "  正文: {chars} 字（{:.1}s）: {}",
            step.elapsed().as_secs_f32(),
            head.replace('\n', " ")
        );

        if chars < 200 {
            println!("  ✗ 正文过短，判定不可用");
            continue;
        }
        println!("  ✓ 通过");
        passed.push(source.clone());
    }

    println!("\n通过 {} / {} 个书源", passed.len(), sources.len());

    if args.import {
        if passed.is_empty() {
            println!("没有可导入的书源");
            return Ok(());
        }
        let db = args.db.context("--import 需要配合 --db=<数据库路径>")?;
        let store = novel_overlay_lib::store::Store::open(&db)
            .map_err(|error| anyhow::anyhow!("打开数据库失败: {error}"))?;
        for source in &passed {
            let id = store
                .upsert_source(source)
                .map_err(|error| anyhow::anyhow!("写入书源失败: {error}"))?;
            println!("已导入 #{id} {}", source.book_source_name);
        }
    }

    Ok(())
}
