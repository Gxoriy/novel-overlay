use novel_overlay_lib::engine::bqg;
use novel_overlay_lib::net::Fetcher;

fn encode_component(input: &str) -> String {
    let mut out = String::new();
    for byte in input.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(*byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[tokio::main]
async fn main() {
    let fetcher = Fetcher::new(2.0, 4).expect("fetcher");
    println!("transport: {}", novel_overlay_lib::net::PageSource::describe(&fetcher));
    for plain in [r#"{"id":106164,"chapterid":1}"#, r#"{"id":3594,"chapterid":68}"#] {
        let token = bqg::encrypt_token(plain).unwrap();
        let api = format!("https://apibi.cc/api/chapter?token={}", encode_component(&token));
        let resp = fetcher
            .request(
                &api,
                &[
                    ("Referer".into(), "https://www.bqg616.cc/".into()),
                    ("Accept".into(), "application/json,text/javascript,*/*;q=0.1".into()),
                ],
            )
            .await
            .expect("req");
        let v: serde_json::Value = serde_json::from_str(&resp.body).expect("json");
        let txt = v.get("txt").and_then(|x| x.as_str()).unwrap_or("");
        let bait = txt.contains("阿拉伯语专业的学生");
        println!(
            "{plain} status={} bait={bait} name={} head={}",
            resp.status,
            v.get("chaptername").and_then(|x| x.as_str()).unwrap_or(""),
            txt.chars().take(60).collect::<String>().replace('\n', " ")
        );
    }
}
