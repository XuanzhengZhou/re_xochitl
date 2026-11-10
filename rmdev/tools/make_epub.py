#!/usr/bin/env python3
"""造一本最小可用的 EPUB（给联调/演示用，不是真书）。

用法：python3 tools/make_epub.py 输出目录 [uuid]
会生成 <uuid>.epub + <uuid>.metadata + <uuid>.content + <uuid>.pagedata，
放进 rM 的库目录后就能被库界面识别成电子书。
"""
import json
import os
import sys
import time
import zipfile

CHAPTERS = [
    ("chapter1.xhtml", "Chapter 1 · The quick brown fox",
     ["The quick brown fox jumps over the lazy dog. " * 6,
      "This is a synthetic EPUB so we can check pagination, text layout and page "
      "turning without shipping a copyrighted book. " * 4]),
    ("chapter2.xhtml", "Chapter 2 · 中文排版也走同一条路径",
     ["这里是一段中文，用来验证断行、标点挤压和分页。官方阅读器在中文上按字断行，"
      "我们的实现也一样：每个汉字都可以是断行点，标点不放行首。" * 4,
      "第二段：Mixed English and 中文 in one paragraph, to make sure line breaking "
      "handles both directions. " * 3]),
    ("chapter3.xhtml", "Chapter 3 · The end",
     ["最后一章很短，用来确认 spine 顺序和目录锚点。"]),
]


def build(outdir: str, uuid: str) -> str:
    os.makedirs(outdir, exist_ok=True)
    epub_path = os.path.join(outdir, f"{uuid}.epub")
    with zipfile.ZipFile(epub_path, "w") as z:
        z.writestr("mimetype", "application/epub+zip", zipfile.ZIP_STORED)
        z.writestr("META-INF/container.xml", """<?xml version="1.0" encoding="UTF-8"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles>
</container>""")
        items, spine, nav = [], [], []
        for i, (name, title, paras) in enumerate(CHAPTERS, start=1):
            body = "".join(f"<p>{p}</p>" for p in paras)
            z.writestr(f"OEBPS/{name}", f"""<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml"><head><title>{title}</title></head>
<body><h1 id="ch{i}">{title}</h1>{body}</body></html>""")
            items.append(f'<item id="c{i}" href="{name}" media-type="application/xhtml+xml"/>')
            spine.append(f'<itemref idref="c{i}"/>')
            nav.append(f'<navPoint id="n{i}" playOrder="{i}"><navLabel><text>{title}</text></navLabel>'
                       f'<content src="{name}"/></navPoint>')
        z.writestr("OEBPS/content.opf", f"""<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0" unique-identifier="i">
<metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
<dc:title>合成样书</dc:title><dc:creator>rmdev</dc:creator>
<dc:language>zh</dc:language><dc:identifier id="i">{uuid}</dc:identifier></metadata>
<manifest>{''.join(items)}<item id="ncx" href="toc.ncx" media-type="application/x-dtbncx+xml"/></manifest>
<spine toc="ncx">{''.join(spine)}</spine></package>""")
        z.writestr("OEBPS/toc.ncx", f"""<?xml version="1.0" encoding="UTF-8"?>
<ncx xmlns="http://www.daisy.org/z3986/2005/ncx/" version="2005-1">
<head><meta name="dtb:uid" content="{uuid}"/></head>
<docTitle><text>合成样书</text></docTitle><navMap>{''.join(nav)}</navMap></ncx>""")

    now = str(int(time.time() * 1000))
    with open(os.path.join(outdir, f"{uuid}.metadata"), "w", encoding="utf-8") as f:
        json.dump({
            "createdTime": now, "lastModified": now, "lastOpened": now,
            "lastOpenedPage": 0, "parent": "", "pinned": False, "deleted": False,
            "synced": True, "type": "DocumentType", "visibleName": "EPUB sample",
            "version": 1,
        }, f, ensure_ascii=False, indent=2)
    with open(os.path.join(outdir, f"{uuid}.content"), "w", encoding="utf-8") as f:
        json.dump({
            "fileType": "epub", "pageCount": 0, "coverPageNumber": -1,
            "documentMetadata": {}, "extraMetadata": {}, "fontName": "NotoSansSC",
            "lineHeight": -1, "orientation": "portrait", "pageTags": [], "tags": [],
            "textAlignment": "justify", "textScale": 1,
        }, f, ensure_ascii=False, indent=2)
    open(os.path.join(outdir, f"{uuid}.pagedata"), "w", encoding="utf-8").write("Blank\n")
    return epub_path


if __name__ == "__main__":
    out = sys.argv[1] if len(sys.argv) > 1 else os.path.expanduser("~/re_xochitl/fixture")
    uid = sys.argv[2] if len(sys.argv) > 2 else "e9b0c1d2-3456-7890-abcd-ef1234567890"
    p = build(out, uid)
    print(p, os.path.getsize(p), "bytes")
