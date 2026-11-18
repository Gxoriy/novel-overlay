"""抓 akarap 书页 /shuwu/251128/ 的目录与正文结构。"""

import re
import ssl
import urllib.parse
import urllib.request

UA = "NovelOverlay/0.1 (personal reading tool; +https://example.invalid/novel-overlay)"
CTX = ssl.create_default_context()
CTX.check_hostname = False
CTX.verify_mode = ssl.CERT_NONE
OPENER = urllib.request.build_opener(urllib.request.HTTPSHandler(context=CTX))


def get(url):
    req = urllib.request.Request(url, headers={"User-Agent": UA})
    with OPENER.open(req, timeout=20) as resp:
        return resp.read().decode("utf-8", "ignore")


def anchors(html):
    return [
        (h, re.sub(r"<[^>]+>", "", t).strip())
        for h, t in re.findall(r'<a\s[^>]*href="([^"]+)"[^>]*>(.*?)</a>', html, re.S | re.I)
    ]


out = []
toc_html = get("https://www.akarap.cc/shuwu/251128/")
out.append(f"===== 书页 len={len(toc_html)} title={re.findall(r'<title>(.*?)</title>', toc_html, re.S)[:1]}")
chapters = [(h, t) for h, t in anchors(toc_html) if re.match(r"^/shuwu/251128/\d+\.html$", h)]
out.append(f"章节 {len(chapters)}: {chapters[:3]}")
pages = [(h, t) for h, t in anchors(toc_html) if re.search(r"ls\d+|page|_\d+", h)]
out.append(f"疑似分页 {len(pages)}: {pages[:6]}")
# 容器
first = chapters[0][0] if chapters else None
if first:
    pos = toc_html.find(first)
    out.append("目录容器: " + re.sub(r"[ \t]+", " ", toc_html[max(0, pos - 900) : pos + 300]))
    with open(r"F:\MyProjet\novel-overlay\probe\akarap-toc.html", "w", encoding="utf-8") as handle:
        handle.write(toc_html)

    chapter_html = get(urllib.parse.urljoin("https://www.akarap.cc/", first))
    text = re.sub(r"<script.*?</script>|<style.*?</style>", " ", chapter_html, flags=re.S)
    text2 = re.sub(r"<[^>]+>", "", text)
    out.append(f"\n===== 正文 len={len(chapter_html)} 汉字 {len(re.findall(r'[\u4e00-\u9fff]', text2))}")
    out.append("title=" + str(re.findall(r"<title>(.*?)</title>", chapter_html, re.S)[:1]))
    for needle in ['id="content"', 'id="booktxt"', 'id="chaptercontent"', 'class="content"', 'id="txt"', 'id="nr1"', 'id="nr"']:
        pos = chapter_html.find(needle)
        if pos >= 0:
            out.append(f"[{needle}] " + re.sub(r"[ \t]+", " ", chapter_html[max(0, pos - 120) : pos + 600]))
            break
    with open(r"F:\MyProjet\novel-overlay\probe\akarap-chapter.html", "w", encoding="utf-8") as handle:
        handle.write(chapter_html)

with open(r"F:\MyProjet\novel-overlay\probe\akarap-toc-analysis.txt", "w", encoding="utf-8") as handle:
    handle.write("\n".join(out))
print("ok")
