"""分析 akarap 搜索结果页卡片结构。"""

import re

html = open(r"F:\MyProjet\novel-overlay\probe\akarap-search.html", encoding="utf-8").read()
out = []
idx = html.find("/shuwu/")
out.append(f"len={len(html)} 首个书链接 @{idx}")
out.append(re.sub(r"[ \t]+", " ", html[max(0, idx - 1500) : idx + 700]))
with open(r"F:\MyProjet\novel-overlay\probe\akarap-search-analysis.txt", "w", encoding="utf-8") as handle:
    handle.write("\n".join(out))
print("ok")
