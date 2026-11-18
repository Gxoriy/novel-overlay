"""检查 akarap 章节分页链接与页码结构。"""

import io
import re

chapter = io.open(r"F:\MyProjet\novel-overlay\probe\akarap-chapter.html", encoding="utf-8").read()
out = []
pages = re.findall(r'href="([^"]*_\d+\.html)"', chapter)
out.append(f"_N.html 链接 {len(set(pages))}: {sorted(set(pages))[:6]}")
idx = chapter.find("下一页")
out.append(f"下一页 @{idx}")
if idx > 0:
    out.append(re.sub(r"[ \t]+", " ", chapter[max(0, idx - 300) : idx + 200]))
# 页码选择/跳转
for needle in ["page_select", "select_page", "第1页", "分页"]:
    pos = chapter.find(needle)
    out.append(f"{needle} @{pos}")

with open(r"F:\MyProjet\novel-overlay\probe\akarap-page.txt", "w", encoding="utf-8") as handle:
    handle.write("\n".join(out))
print("ok")
