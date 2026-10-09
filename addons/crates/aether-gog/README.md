# aether-gog

GOG Galaxy content-system v2 access for the aether add-ons:

- **Login.** `login_url(&cfg)` is the address to open in a browser. After
  logging in, GOG shows a page at
  `https://embed.gog.com/on_login_success?origin=client&code=…`; paste that
  whole address (or just its `code`) into `complete_login`, which exchanges it
  for an access and a refresh token and saves them to `cfg.credentials`
  (JSON, mode 0600 in a 0700 directory; tokens are zeroized in memory on drop).
  An access token is refreshed a minute before it expires; one GOG rejects
  (401) is refreshed exactly once and the request retried. A refused refresh is
  `GogError::LoginExpired`, whose message says to log in to GOG again.
- **`GogContent`.** `builds(product, os)` lists a product's v2 builds;
  `build_details(&build)` gives its depots (some from DLC product ids);
  `depot(&depot_ref)` gives a depot manifest (files, chunks, the small-files
  container), cached on disk under `{cache_dir}/manifests`.
- **`GogDepotFile`.** `read_at(offset, buf)` maps the range to the chunks that
  cover it, fetches them from the product's secure CDN link (cached until it
  expires), checks the compressed MD5, inflates, checks the inflated MD5 and
  splices. A damaged chunk is retried per `aether_net::RetryPolicy` and then
  reported as `SourceError::CorruptPart`; it is never served or cached.
  Inflated chunks are kept in an in-memory LRU (64 chunks / 256 MiB by
  default). `into_blocking(handle)` gives a synchronous
  `aether_archive::RangeRead`.

Every endpoint, and the OAuth client, is a `GogConfig` field, so tests run
against a local fake (`tests/fake_gog.rs`). The default client is GOG Galaxy's
own (`46899977096215655`, redirect `https://embed.gog.com/on_login_success?origin=client`),
the one heroic-gogdl, lgogdownloader and minigalaxy use for a paste-the-URL
login; NexusMods.App's client is registered for its `nxm://gog-auth` redirect
and does not fit a terminal flow.

Out of scope: installers (makeself / mojosetup, Windows setups), the v1
content system, patches.

## Testing against real GOG

The live tests (`tests/live.rs`) are ignored by default. From `addons/`, log
in once; the `gog_login` example prints the login address, reads back the
address of the page GOG lands on (`https://embed.gog.com/on_login_success?…code=…`;
the code works once and only for a few minutes) and saves the tokens:

```sh
cargo run -p aether-gog --example gog_login -- ~/.config/aether-gog/creds.json
```

Then run the tests against a product the account owns (`GOG_PRODUCT`, default
1711230643, The Elder Scrolls V: Skyrim Special Edition):

```sh
GOG_CREDENTIALS=~/.config/aether-gog/creds.json GOG_PRODUCT=1711230643 \
  cargo test -p aether-gog --test live -- --ignored --nocapture
```

They refresh the token, list the product's builds and read the first 64 KiB of
one depot file. A product the account does not own fails the read with
`GogError::NotOwned`.

## Attribution

Ported from NexusMods.App (`src/NexusMods.Networking.GOG`, GPL-3.0),
https://github.com/Nexus-Mods/NexusMods.App. Endpoint and URL-template details
were checked against heroic-gogdl (https://github.com/Heroic-Games-Launcher/heroic-gogdl,
GPL-3.0) and minigalaxy (https://github.com/sharkwouter/minigalaxy, GPL-3.0).
