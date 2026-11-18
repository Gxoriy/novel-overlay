"""精确定位 akarap 目录列表与正文容器。"""

import io
import re

toc = io.open(r"F:\MyProjet\novel-overlay\probe\akarap-toc.html", encoding="utf-8").read()
chapter = io.open(r"F:\MyProjet\novel-overlay\probe\akarap-chapter.html", encoding="utf-8").read()

out = []
out.append("===== 目录页全部 ul =====")
for m in re.finditer(r"<ul[^>]*>", toc):
    out.append("  " + re.sub(r"\s+", " ", m.group(0))[:100])

# 最新章节 3560100 所在容器
idx = toc.find("/shuwu/251128/3560100.html")
out.append(f"\n3560100 @{idx}")
out.append(re.sub(r"[ \t]+", " ", toc[max(0, idx - 700) : idx + 200]))

# 正文章节列表开头（3553413 上架感言）
idx2 = toc.find("/shuwu/251128/3553413.html")
out.append(f"\n正序列表起点 @{idx2}")
out.append(re.sub(r"[ \t]+", " ", toc[max(0, idx2 - 500) : idx2 + 120]))

out.append("\n===== 正文页 word_read 内部 =====")
pos = chapter.find('class="word_read"')
out.append(re.sub(r"[ \t]+", " ", chapter[pos : pos + 1800]))

with open(r"F:\MyProjet\novel-overlay\probe\akarap-precise.txt", "w", encoding="utf-8") as handle:
    handle.write("\n".join(out))
print("ok")
