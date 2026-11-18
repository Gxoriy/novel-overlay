"""定位 akarap 正文容器与目录容器结构。"""

import re
import ssl
import urllib.request

UA = "NovelOverlay/0.1 (personal reading tool; +https://example.invalid/novel-overlay)"
CTX = ssl.create_default_context()
CTX.check_hostname = False
CTX.verify_mode = ssl.CERT_NONE
OPENER = urllib.request.build_opener(urllib.request.HTTPSHandler(context=CTX))

chapter = open(r"F:\MyProjet\novel-overlay\probe\akarap-chapter.html", encoding="utf-8").read()
toc = open(r"F:\MyProjet\novel-overlay\probe\akarap-toc.html", encoding="utf-8").read()

out = []
out.append("===== 正文页全部标签（含 id/class）=====")
for m in re.finditer(r"<(div|article|section|td)[^>]*>", chapter):
    tag = m.group(0)
    if "id=" in tag or "class=" in tag:
        out.append("  " + re.sub(r"\s+", " ", tag)[:110])

# 找含正文的长文本块：定位「第一章附近」文本的位置
idx = chapter.find("斗气大陆")
out.append(f"\n'斗气大陆' @ {idx}")
if idx >= 0:
    out.append(re.sub(r"[ \t]+", " ", chapter[max(0, idx - 800) : idx + 400]))

# 目录容器：找章节列表 ul/div
out.append("\n===== 目录页章节容器 =====")
pos = toc.find("/shuwu/251128/3553413.html", 5000)
while pos > 0:
    ctx = toc[max(0, pos - 400) : pos]
    if "<li" in ctx or "<dd" in ctx or "list" in ctx.lower():
        out.append(re.sub(r"[ \t]+", " ", toc[max(0, pos - 700) : pos + 200]))
        break
    pos = toc.find("/shuwu/251128/3553413.html", pos + 1)

with open(r"F:\MyProjet\novel-overlay\probe\akarap-containers.txt", "w", encoding="utf-8") as handle:
    handle.write("\n".join(out))
print("ok")
