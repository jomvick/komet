# Third-party notices

Komet bundles the following syntax-highlighting components. Their parsers and
queries are consumed from the pinned Rust crates listed in `Cargo.lock`.

| Component | Version | License | Source |
| --- | --- | --- | --- |
| Tree-sitter | 0.26.11 | MIT | https://github.com/tree-sitter/tree-sitter |
| Tree-sitter highlight | 0.26.11 | MIT | https://github.com/tree-sitter/tree-sitter |
| Tree-sitter Rust grammar and queries | 0.24.2 | MIT | https://github.com/tree-sitter/tree-sitter-rust |
| Tree-sitter JavaScript grammar and queries | 0.25.0 | MIT | https://github.com/tree-sitter/tree-sitter-javascript |
| Tree-sitter TypeScript grammar and queries | 0.23.2 | MIT | https://github.com/tree-sitter/tree-sitter-typescript |
| Tree-sitter Python, Go, JSON, Bash, HTML, CSS, C, C++, C#, Java, Ruby and PHP grammars and queries | pinned in `Cargo.lock` | MIT | https://github.com/tree-sitter |
| Tree-sitter TOML, Markdown, YAML, Kotlin, Swift, SQL, Lua, Nix, Make and Containerfile grammars and queries | pinned in `Cargo.lock` | MIT-compatible; see each crate | Crate repositories recorded in `Cargo.lock` |
| komet (upstream) | — | MIT | https://github.com/komet/komet — Komet is a direct fork; upstream logic ported per `crates/doc/src/parts.rs` and harness adapters |
| LobeHub Icons (`@lobehub/icons-static-svg`) | 1.101.0 | MIT | https://github.com/lobehub/lobe-icons — selected AI harness and model-provider SVGs |

The full Komet distribution remains licensed under the terms in `LICENSE`.

LobeHub Icons license:

```text
MIT License

Copyright (c) 2023 LobeHub

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```
