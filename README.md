<div align="center">
    <img src="assets/hero-notext.webp" width=200>
    <h1>nixsearch</h1>
</div>

The Nix ecosystem has thousands of packages and options. Many solutions exist to help with rummaging through these options, and after running into frustrations with them, this is mine.

### Goals

- [x] Fast search core built with [tantivy](https://github.com/quickwit-oss/tantivy)
  - Support a wide array of producer strategies, so many data sources can be easily mixed together
- [x] Magical web frontend leveraging [Datastar](https://data-star.dev/)
  - https://nixsearch.thekoppe.com
  - Run it yourself: [example config](/nixsearch.example.toml)
- [ ] MCP frontend, to easily feed agents with options & source code

### Development

This section should be expanded on. But utilize the devenv defined in this flake with `nix develop .#`. Then, the cli can be run with `cargo run -p nixsearch -- <command>`.

## Inspirations

This section can also be though of as "Motivations", since these past projects are what led to nixsearch.

- [nixos-search](https://github.com/nixos/nixos-search): The official solution. Limited to primarily Nixpkgs & NixOS options.
- [Searchix](https://codeberg.org/alinnow/searchix): I was a heavy Searchix user, and nixsearch is really just exactly what I wanted out of Searchix. <br/>Some key differences are:
  - Reliability
    - I would have frequent issues loading pages on Searchix, especially on unreliable internet
  - Better search algorithm
    - Tantivy provides a very strong search foundation, allowing nixsearch to resemble the powerful Grasshopper-powered search of `nixos-search` without compromises.
  - Refs as a primitive
    - Searchix doesn't have a solution to deal with multiple refs of the same source, such as `25.11` and `unstable`.
- [NuschtOS/search](https://github.com/NuschtOS/search): Another solution that focuses on building everything statically.

## AI Disclosure

AI was utilized heavilty in the development of this project. But LLMs still can't manage to do even a search bar on their own, so nearly all code has gone through human eyes and hands. See the [PLAN.md](/PLAN.md) document to understand how context was initially scaffolded for this codebase.

Thanks for checking out nixsearch!
