from pathlib import Path
import re

_INCLUDE = re.compile(r'include!\("([^"]+)"\);')


def read_rust_module(root: Path, relative: str) -> str:
    """Read a Rust module and each local source section it includes."""
    path = root / relative
    text = path.read_text(encoding="utf-8")
    parts = [text]
    for include in _INCLUDE.findall(text):
        nested = path.parent.relative_to(root) / include
        parts.append(read_rust_module(root, str(nested)))
    return "\n".join(parts)
