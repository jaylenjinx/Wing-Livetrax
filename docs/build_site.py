#!/usr/bin/env python3
"""Inline the screenshots into one self-contained HTML file.

The site in this folder is what GitHub Pages serves, images and all. This
produces the same page as a single file, for sharing somewhere that has no
place to put the images alongside it.

    python3 docs/build_site.py [output.html]
"""
import base64, mimetypes, os, re, sys

HERE = os.path.dirname(os.path.abspath(__file__))

def main():
    out_path = sys.argv[1] if len(sys.argv) > 1 else os.path.join(HERE, "site.html")
    html = open(os.path.join(HERE, "index.html")).read()

    def inline(match):
        name = match.group(1)
        path = os.path.join(HERE, name)
        if not os.path.isfile(path):
            print(f"missing image: {name}", file=sys.stderr)
            return match.group(0)
        mime = mimetypes.guess_type(path)[0] or "image/png"
        data = base64.b64encode(open(path, "rb").read()).decode()
        return f'src="data:{mime};base64,{data}"'

    html = re.sub(r'src="([^"]+\.(?:png|jpg|jpeg|svg))"', inline, html)
    open(out_path, "w").write(html)
    print(f"wrote {out_path} ({len(html) / 1_048_576:.1f} MB)")

if __name__ == "__main__":
    main()
