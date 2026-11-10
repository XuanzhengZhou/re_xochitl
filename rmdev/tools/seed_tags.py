#!/usr/bin/env python3
"""给本机假库（`~/re_xochitl/fixture`）写上标签，供 `sh tools/render.sh tags` 出图。

官方 `.content` 里有两处标签：
  * `tags`: 文档级，`["Ideas", ...]`
  * `pageTags`: 页面级，`[{"pageId": "<uuid>", "tags": ["a"]}]`

跑法：`python3 tools/seed_tags.py [库目录]`（默认 ~/re_xochitl/fixture）。
幂等：重复跑结果一样。空态要看 `RMDEV_LIB_ROOT=/tmp/empty sh tools/render.sh tags`。
"""
import json
import os
import sys

DOCS = {
    # 4 个标签 → 缩略图上显示 3 个 + "+1"
    "1554f45c-b223-40d2-adf7-d436232fa56a": {
        "tags": ["Ideas", "Sketch", "To do", "Writing"],
        "pages": {0: ["Idea"], 1: ["Idea", "Draft"], 2: ["Sketch"]},
    },
    "c2981746-1392-46f4-9a08-a7b6f95de185": {
        "tags": ["Research", "Reading"],
        "pages": {0: ["Research"], 3: ["Important"], 6: ["Quote"]},
    },
}


def main() -> int:
    root = sys.argv[1] if len(sys.argv) > 1 else os.path.expanduser("~/re_xochitl/fixture")
    for uuid, spec in DOCS.items():
        path = os.path.join(root, uuid + ".content")
        with open(path) as f:
            content = json.load(f)
        ids = [p["id"] for p in content.get("cPages", {}).get("pages", [])]
        content["tags"] = spec["tags"]
        page_tags = []
        for idx, tags in spec["pages"].items():
            if idx >= len(ids):
                print(f"[seed] {uuid} 只有 {len(ids)} 页，跳过第 {idx + 1} 页")
                continue
            page_tags.append({"pageId": ids[idx], "tags": tags})
        content["pageTags"] = page_tags
        with open(path, "w") as f:
            json.dump(content, f, ensure_ascii=False, indent=4)
        print(f"[seed] {uuid}: tags={spec['tags']} pageTags={len(page_tags)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
