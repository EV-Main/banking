{
  inputs = {
    v_flakes.url = "github:valeratrades/v_flakes?ref=v1.6";
  };
  outputs = { self, v_flakes }:
    let
      inherit (v_flakes) flake-utils pre-commit-hooks rust-overlay;
    in
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import v_flakes.default_nixpkgs {
          inherit system;
          overlays = [ (import rust-overlay) ];
          config.allowUnfree = true;
        };
        # Canonical toolchain pinned in v_flakes — byte-identical across repos, so
        # the nix store dedups it and sccache cross-references compilations.
        rust = v_flakes.rs.default_nightly system;
        pre-commit-check = pre-commit-hooks.lib.${system}.run (v_flakes.files.preCommit { inherit pkgs; });
        pname = "ev_banking";
        # ── ev_invest dev topology (single source of truth for ports) ───────
        # ONE postgres + ONE redis serve every sibling repo (concierge and
        # site_conductor mirror these values in their flakes); tigerbeetle is
        # banking-only. Postgres database name == app name. Banking's gRPC planes
        # cluster on 5005x, web UIs on 5006x (site_conductor: 50063/50064);
        # concierge lives on its own port. Redis has no named dbs — numeric
        # mapping: 0=banking, 1=concierge.
        ports = {
          POSTGRES_PORT = "5432";
          REDIS_PORT = "6379";
          TIGERBEETLE_PORT = "3033";
          PIGGYBANK_CORE_PORT = "50051";
          PIGGYBANK_AUTH_PORT = "50052";
          SIGNER_PORT = "50053";
          CONCIERGE_PORT = "55670";
          CABINET_FRONTEND_PORT = "50061";
          CABINET_BACKEND_PORT = "50062";
        };
        # DEFAULTS, not overrides: anything already set in the environment (or a
        # sourced `.env`) wins — machines with non-standard ports stay working.
        portEnv = pkgs.lib.concatStrings (pkgs.lib.mapAttrsToList (n: v: "export ${n}=\"\${${n}:-${v}}\"\n") ports);

        rs = v_flakes.rs { inherit pkgs rust; };
        github = v_flakes.github {
          inherit pkgs pname rs;
          enable = true;
          # Public repo → public Cachix (`ev-invest`); pull deps + push built paths.
          cache = { cachix = "ev-invest"; };
          lastSupportedVersion = "nightly-2026-05-12";
          containerRelease = { registry = "ghcr.io/ev-invest"; };
          gitignore.extra = ''
            ## Local Postgres
            .pg/
            ## Local TigerBeetle
            .tb/
            .tb-client
            ## Local Redis
            .redis/
            ## Node / Turborepo
            node_modules/
            .next/
            .turbo/
            ## Env
            .env
            .env.local
            ## LLMs
            AGENTS.md
            CLAUDE.md
            .claude/
          '';
          jobs = {
            warnings.augment = [ "tokei" "code-duplication" ];
            other.augment = [ "loc-badge" ];
          };
          lfs = true;
        };
        readme = v_flakes.readme-fw {
          inherit pkgs pname;
          defaults = true;
          lastSupportedVersion = "nightly-1.92";
          rootDir = ./.;
          # crates_io (unpublished), loc (no gist) and ci (private repo) all
          # render "not found" — keep only the badges that resolve. The wasm
          # badge is injected in the devShell shellHook (no generator key for it).
          badges = [ "msrv" "docs_rs" ];
        };
        combined = v_flakes.utils.combine { inherit rust; modules = [ rs github readme ]; };

        # ── shared shims ────────────────────────────────────────────────────
        # rust-lld (wasm32 linker) embeds the wrong rpath on macOS — it looks for
        # libLLVM.dylib in bin/../lib/ but Nix puts it one level up in lib/.
        # The FALLBACK var only kicks in when normal resolution fails — exactly
        # rust-lld's case, never clang's (which would otherwise be forced onto
        # rustc's older libLLVM when linking host proc-macros).
        dyldFallback = ''export DYLD_FALLBACK_LIBRARY_PATH="${rust}/lib''${DYLD_FALLBACK_LIBRARY_PATH:+:$DYLD_FALLBACK_LIBRARY_PATH}"'';
        # tonic-build / prost-build shell out to protoc; point them at nixpkgs'.
        protocEnv = ''export PROTOC="${pkgs.protobuf}/bin/protoc"'';

        # ── TigerBeetle Rust client assets ──────────────────────────────────
        # Upstream's precompiled zig (fully static on linux, system-libs-only on
        # macOS) instead of nixpkgs zig, whose dynamic libLLVM turns the one-time
        # client build into a multi-GB download. The official tarball is ~50MB.
        zigBin =
          let
            dist = {
              x86_64-linux = { suffix = "x86_64-linux"; sha256 = "24aeeec8af16c381934a6cd7d95c807a8cb2cf7df9fa40d359aa884195c4716c"; };
              aarch64-linux = { suffix = "aarch64-linux"; sha256 = "f7a654acc967864f7a050ddacfaa778c7504a0eca8d2b678839c21eea47c992b"; };
              x86_64-darwin = { suffix = "x86_64-macos"; sha256 = "b0f8bdfb9035783db58dd6c19d7dea89892acc3814421853e5752fe4573e5f43"; };
              aarch64-darwin = { suffix = "aarch64-macos"; sha256 = "39f3dc5e79c22088ce878edc821dedb4ca5a1cd9f5ef915e9b3cc3053e8faefa"; };
            }.${system};
          in
          pkgs.stdenvNoCC.mkDerivation {
            pname = "zig-bin";
            version = "0.14.1";
            src = pkgs.fetchurl {
              url = "https://ziglang.org/download/0.14.1/zig-${dist.suffix}-0.14.1.tar.xz";
              inherit (dist) sha256;
            };
            dontConfigure = true;
            dontBuild = true;
            dontFixup = true;
            installPhase = ''
              mkdir -p $out/bin
              cp zig $out/bin/
              cp -r lib $out/lib
            '';
          };

        # Official precompiled server binary. nixpkgs' tigerbeetle lags behind and
        # a cluster evicts any client released after it (client_release_too_high) —
        # the server must be >= the 0.17.6 client built below. The release binaries
        # are static (zig-built), so they run on NixOS unpatched. Parametrized by
        # target system (fetch+unzip runs anywhere): `new-replica` below ships the
        # binary matching a remote host's arch, not the local one.
        tigerbeetleBinFor = targetSystem:
          let
            dist = {
              x86_64-linux = { file = "tigerbeetle-x86_64-linux.zip"; hash = "sha256-butV+rwsBnpLCCOV9KNzvCNCC8QbG/AR7ZRnl+Uyl7Y="; };
              aarch64-linux = { file = "tigerbeetle-aarch64-linux.zip"; hash = "sha256-JmsczIvW67WTrK0iCEDHcu9lhMyK84ZvhIs+lgL2bAs="; };
              x86_64-darwin = { file = "tigerbeetle-universal-macos.zip"; hash = "sha256-83nhQqHYu6PPKu4rH6rjD/J3hJinhXQ6b7C4hZ9//v8="; };
              aarch64-darwin = { file = "tigerbeetle-universal-macos.zip"; hash = "sha256-83nhQqHYu6PPKu4rH6rjD/J3hJinhXQ6b7C4hZ9//v8="; };
            }.${targetSystem};
          in
          pkgs.stdenvNoCC.mkDerivation {
            pname = "tigerbeetle-bin";
            version = "0.17.6";
            src = pkgs.fetchurl {
              url = "https://github.com/tigerbeetle/tigerbeetle/releases/download/0.17.6/${dist.file}";
              inherit (dist) hash;
            };
            nativeBuildInputs = [ pkgs.unzip ];
            unpackPhase = "unzip $src";
            dontConfigure = true;
            dontBuild = true;
            dontFixup = true;
            installPhase = ''
              mkdir -p $out/bin
              install -m755 tigerbeetle $out/bin/
            '';
          };
        tigerbeetleBin = tigerbeetleBinFor system;

        # Builds the native C client library + header so the official tigerbeetle
        # Rust crate can link against them. The output is the src/clients/rust/
        # directory with compiled assets in place, ready as a Cargo path dep.
        tigerbeetleClient = pkgs.stdenv.mkDerivation {
          name = "tigerbeetle-client";
          src = pkgs.fetchzip {
            url = "https://github.com/tigerbeetle/tigerbeetle/archive/refs/tags/0.17.6.tar.gz";
            hash = "sha256-b519nsDbas+XOw3ulAnzpk2KwtJkeOC3e13urM2tUSM=";
          };
          # TigerBeetle pins its zig version exactly (0.17.6 → zig 0.14.1);
          # newer zig is rejected by build.zig with @compileError.
          nativeBuildInputs = [ zigBin pkgs.git ];
          # build.zig runs `git tag --merged HEAD^` at configure time and
          # unconditionally consumes 4+ version-shaped tags; the release tarball
          # has no .git, so fabricate a history with enough tags. They only feed
          # lazily-evaluated fetch_release steps that clients:rust never depends on.
          postPatch = ''
            git init -q
            git -c user.name=nix -c user.email=nix@localhost add -A
            git -c user.name=nix -c user.email=nix@localhost commit -qm base
            for v in 0.17.1 0.17.2 0.17.3 0.17.4 0.17.5; do git tag "$v"; done
            git -c user.name=nix -c user.email=nix@localhost commit -qm head --allow-empty
          '';
          buildPhase = ''
            export ZIG_GLOBAL_CACHE_DIR="$TMPDIR/zig-cache"
            zig build clients:rust -Drelease \
              -Dgit-commit=64899c7a41fd3d74c68da7bb2efcb7d208abd5f2 \
              -Dconfig-release=0.17.6 -Dconfig-release-client-min=0.17.6
          '';
          installPhase = ''
            mkdir -p $out
            cp -r src/clients/rust/* $out/
            printf '\n[workspace]\n' >> $out/Cargo.toml
          '';
        };

        # Symlink the TigerBeetle Rust client (with pre-built native assets) so
        # the path dependency in the workspace Cargo.toml resolves. Lives at the
        # repo root, NOT under a member dir: cargo's workspace exclude can never
        # match a path inside a member's directory.
        linkTbClient = ''
          tb_client_dir="$(git rev-parse --show-toplevel)/.tb-client"
          if [ ! -L "$tb_client_dir" ] || [ "$(readlink "$tb_client_dir")" != "${tigerbeetleClient}" ]; then
            rm -rf "$tb_client_dir"
            ln -s "${tigerbeetleClient}" "$tb_client_dir"
          fi
        '';

        # ── production binaries + OCI images ────────────────────────────────
        # Lean toolchain for release image builds: rustc + cargo + std only, keeping
        # the fat dev toolchain out of the release closure (mirrors site_conductor).
        rustBuild = pkgs.rust-bin.selectLatestNightlyWith (toolchain: toolchain.minimal);
        rustPlatformBuild = pkgs.makeRustPlatform { cargo = rustBuild; rustc = rustBuild; };
        buildSrc = pkgs.lib.cleanSourceWith {
          src = ./.;
          # .cargo holds dev-only accelerators (mold, nightly -Z flags) the hermetic
          # sandbox lacks; .tb-client is recreated in-sandbox below.
          filter = path: _type:
            ! builtins.elem (baseNameOf path) [ "target" "node_modules" ".next" ".turbo" ".tb-client" ".tb" ".redis" ".pg" ".direnv" ".git" ".cargo" "tmp" "docs" "result" ];
        };
        # ONE derivation for all three service binaries — they share the workspace
        # dep graph, so separate buildRustPackage calls would triple the CI compile.
        bankingBins = rustPlatformBuild.buildRustPackage {
          pname = "${pname}-bins";
          version = (builtins.fromTOML (builtins.readFile ./piggybank/core/Cargo.toml)).package.version;
          src = buildSrc;
          cargoLock = {
            lockFile = ./Cargo.lock;
            # evconcierge_* is a rev-pinned public git dep — builtin fetchGit needs no hash.
            allowBuiltinFetchGit = true;
          };
          postPatch = "ln -sfn ${tigerbeetleClient} .tb-client";
          cargoBuildFlags = [ "-p" "piggybank-core" "-p" "piggybank-signer" "-p" "cabinet-backend" ];
          nativeBuildInputs = with pkgs; [ protobuf pkg-config ];
          buildInputs = [ pkgs.openssl ];
          PROTOC = "${pkgs.protobuf}/bin/protoc";
          doCheck = false;
        };

        # The PostHog PROJECT key for the cabinet's product analytics (`phc_…`). Public by
        # construction — it ships in the browser bundle to every visitor and only lets a
        # client ingest events, never read them — so it is a literal here, the same way the
        # Sentry DSN below is. Empty until the owner pastes the project's key: an empty
        # string reaches `config.ts` as "unset", and the cabinet's analytics stays a no-op
        # (no posthog-js loaded, no `identify`, no events) rather than talking to a wrong
        # project. Must be the SAME key the site (site_conductor) uses, so one PostHog
        # person carries a visitor from the landing to the first subscription.
        posthogKey = "phc_sBwWEgdgockVmfyucBRkTTo6iZ4Y2eApSGorD22WLzj3";

        # ── cabinet production image (Next.js standalone, npm workspace build) ──
        cabinetApp = pkgs.buildNpmPackage {
          pname = "${pname}-cabinet";
          version = (builtins.fromJSON (builtins.readFile ./cabinet/frontend/package.json)).version;
          src = buildSrc;
          # build node_modules straight from the root package-lock.json — no FOD hash to drift.
          npmDeps = pkgs.importNpmLock { npmRoot = ./.; };
          npmConfigHook = pkgs.importNpmLock.npmConfigHook;
          env = {
            NEXT_TELEMETRY_DISABLED = "1";
            # next.config rewrites resolve at BUILD time (routes-manifest.json), so the
            # BFF's in-cluster service DNS is baked here; matches the contract below.
            CABINET_BACKEND_URL = "http://ev-banking-cabinet-backend:50062";
            # Browser error monitoring. A NEXT_PUBLIC_* value is inlined at BUILD time, so
            # this cannot come from the pod's env the way the server-side secrets do — and
            # the same inlining is what lets `assertConfig()` see it at boot. A Sentry
            # PUBLIC dsn is not a secret: it ships to every browser that loads the app, so
            # it lives here as a literal rather than forcing an impure build for a value
            # that is public anyway. `shared/config/security.ts` derives the CSP
            # `connect-src` origin from it, so the browser is allowed to report.
            NEXT_PUBLIC_SENTRY_DSN = "https://702d594df36cfbd3c5a711613d3981e7@o4511508657012736.ingest.de.sentry.io/4511508677066832";
            # Product analytics, same build-time inlining as the DSN (see `posthogKey`). The
            # host is pinned alongside the key on purpose: `shared/config/security.ts` puts
            # its origin into the CSP `connect-src`, and without it a configured key would
            # make posthog-js default to this very host — and the CSP would then block it.
            NEXT_PUBLIC_POSTHOG_KEY = posthogKey;
            NEXT_PUBLIC_POSTHOG_HOST = "https://us.i.posthog.com";
            # Where Home's "Service-Arb panel" card points. Public, and inlined at build
            # like the two above; unset, the card is simply not shown.
            NEXT_PUBLIC_SA_PANEL_URL = "https://sa.evinvest.ltd";
            # Cloudflare Turnstile site key of widget `evinvest sign-in` (Ev Invest account),
            # for the sign-in dialog. Public like the DSN; its secret is concierge's.
            NEXT_PUBLIC_TURNSTILE_SITE_KEY = "0x4AAAAAAFSnp3-3WadT45qx";
          };
          buildPhase = ''
            runHook preBuild
            npm run build --workspace @evbanking/cabinet
            runHook postBuild
          '';
          # Monorepo standalone layout: server.js lands under the workspace subdir.
          installPhase = ''
            runHook preInstall
            test -f cabinet/frontend/.next/standalone/cabinet/frontend/server.js \
              || { echo "standalone server.js not at the expected monorepo path" >&2; find cabinet/frontend/.next/standalone -maxdepth 3 -name server.js >&2; exit 1; }
            mkdir -p $out
            cp -r cabinet/frontend/.next/standalone/. $out/
            cp -r cabinet/frontend/.next/static $out/cabinet/frontend/.next/static
            cp -r cabinet/frontend/public $out/cabinet/frontend/public
            runHook postInstall
          '';
          dontNpmInstall = true;
        };

        # The BFF serves /api/mfe-registry from this file; env points at the image path.
        mfeRegistryRoot = pkgs.runCommand "mfe-registry" { } ''
          mkdir -p $out
          cp ${./cabinet/frontend/mfe-registry.json} $out/mfe-registry.json
        '';

        tbProd = import ./deploy/tigerbeetle.nix;
        # Secret env (signing key, JWKS, issuance/bridge tokens, WALLET_KEK) arrives
        # via the k8s Secrets gitops/k3s own — never baked into contracts or images.
        containerStd = v_flakes.container.implement {
          inherit pkgs pname;
          containers.piggybank = {
            port = 50051;
            # gRPC-only server: gitops swaps these httpGet probes for native `grpc`
            # probes; the path is a required contract placeholder.
            healthPath = "/";
            criticality = "normal";
            entrypoint = [ "/bin/piggybank" ];
            contents = [ bankingBins ];
            env = {
              DATABASE_URL = "postgres://evinvest@10.42.0.1:5432/banking";
              REDIS_URL = "redis://10.42.0.1:6379/0";
              # On-chain rail gates (Rails::from_env). Per-rail API keys arrive via
              # k8s Secrets; main.rs refuses a prod boot with every rail empty.
              TON_API_URL = "https://toncenter.com/api/v3";
              #
              # The four EVM RPC URLs are DELIBERATELY ABSENT from this set. They now carry
              # an access token in their path, which makes them secrets, and a secret cannot
              # live in a committable file. They come from the `kubernetes-ev-banking-piggybank`
              # Secret via `envFrom` instead:
              #
              #   BSC_RPC_URL · BSC_LOGS_RPC_URL · POLYGON_RPC_URL · POLYGON_LOGS_RPC_URL
              #
              # Listing any of them here again would not merely duplicate the value — an
              # explicit `env` entry **overrides** `envFrom`, so the plain URL would silently
              # win over the keyed one and the rail would keep running unauthenticated.
              # That is why they are commented out rather than left with a keyless default:
              # a default here is indistinguishable, at runtime, from the Secret not working.
              #
              # The token buys HISTORY, and only some. Measured against the watcher's real
              # query shape (a `to`-topic filter over the USDT contract), not provider docs,
              # which are wrong about all of this:
              #
              #   BSC keyless   — 500-block ranges answer to ~5k deep (~1h at 0.75s/block);
              #                   beyond that, "Archive requests require a personal token".
              #   BSC keyed     — the same ranges answer to ~50k deep (~10h); at 200k the
              #                   node says "History has been pruned for this block", so it
              #                   is a pruned full node, NOT an archive. Backfilling a gap
              #                   older than ~10h still needs a paid archive plan.
              #   Polygon keyed — serves getLogs, which the keyless publicnode refused
              #                   outright, at 500-block ranges to ~50k deep. This retires
              #                   the drpc split AND the POLYGON_MAX_BLOCK_RANGE=100 override
              #                   drpc forced (its real cap was a 100-block range, whatever
              #                   its "10000 blocks" error text claimed), so the rail catches
              #                   up 5x faster on the code default of 500.
              #
              # Re-measure before trusting any of this after a provider change; the probe is
              # in docs/RUNBOOK-polygon-rail.md.
              TIGERBEETLE_ADDRESS = pkgs.lib.concatStringsSep "," tbProd.addresses;
              TIGERBEETLE_CLUSTER_ID = tbProd.clusterId;
              AUTH_SIGNING_KID = "prod-1";
              RUST_LOG = "info";
              # The canonical prod topology (secret-free, committable): the binary
              # reads env only (ev_lib::settings), and gitops' Deployment env
              # carries the same values — keep the two in step.
              GRPC_ADDR = "0.0.0.0:50051";
              AUTH_GRPC_ADDR = "0.0.0.0:50052";
              SIGNER_GRPC_ADDR = "http://127.0.0.1:50053";
              # The lifecycle bridge dials concierge's TLS listener (55672, not the h2c
              # 55670 the cabinet BFF still uses) and pins its CA: in production the hub
              # REFUSES to boot on a cleartext or unpinned bridge (EV-invest/banking#199,
              # phase 2). devops mounts every key of the `kubernetes-ev-banking-piggybank`
              # Secret as a file at /etc/settings/<KEY>; BRIDGE_TLS_CA_PEM is written into
              # that Secret from sops (rpi5.nix scopes.nix). Rollout order, and it is not
              # optional: rpi5.nix secrets + rebuild → concierge release with the listener
              # → devops Service/NetworkPolicy for 55672 + this pin → THIS banking release
              # last. Shipped ahead of any of those, the hub refuses to start, by design.
              CONCIERGE_BRIDGE_ADDR = "https://concierge:55672";
              BRIDGE_TLS_CA_PEM_FILE = "/etc/settings/BRIDGE_TLS_CA_PEM";
              APP_ENV = "production";
            };
          };
          containers.signer = {
            # Image-only contract: gitops mounts this as the piggybank pod's sidecar
            # (reusing this env) and gives it no probes of its own.
            port = 50053;
            healthPath = "/";
            criticality = "normal";
            entrypoint = [ "/bin/signer" ];
            contents = [ bankingBins ];
            env = {
              SIGNER_DATABASE_URL = "postgres://evinvest@10.42.0.1:5432/banking_signer";
              SIGNER_GRPC_ADDR = "127.0.0.1:50053";
              AUTH_JWKS_GRPC_ENDPOINT = "http://127.0.0.1:50052";
              APP_ENV = "production";
              RUST_LOG = "info";
              # The signer's OWN cap on a single treasury transfer, in whole USDT. This is the
              # second gate: it holds even if the hub is compromised, because the signer is a
              # separate trust domain and applies it to every treasury-sourced payout itself.
              # Until this was set the signer logged `signer spend policy inactive` on every
              # boot and one forged request could drain the whole hot wallet.
              #
              # Size it to the HOT FLOAT, not to ambition: the cap only protects what it is
              # smaller than. Raise it deliberately as liquidity grows — it is one number, and
              # a payout above it fails closed with a clear permission_denied, never silently.
              SIGNER_MAX_TRANSFER_USDT = "100";
              # SIGNER_DESTINATION_ALLOWLIST is deliberately NOT set. It restricts treasury
              # transfers to pre-registered addresses, and user withdrawals go to arbitrary
              # addresses — enabling it would reject every legitimate withdrawal. It belongs to
              # a staged/hardened posture (treasury → known addresses only), not to this one.
            };
          };
          containers.cabinet-backend = {
            port = 50062;
            # The SHALLOW check on purpose: `healthPath` feeds all three kubelet probes, and
            # `/api/health` (the deep smoke path) forwards to piggybank's gRPC health. With one
            # replica, a probe that depends on piggybank turns every piggybank rollout into a
            # NotReady BFF: the zone proxy meets ECONNREFUSED and answers a bodyless 500, which
            # the cabinet renders as "temporarily unavailable" until the operator reloads.
            healthPath = "/api/health/live";
            criticality = "normal";
            entrypoint = [ "/bin/cabinet-backend" ];
            contents = [ bankingBins mfeRegistryRoot ];
            env = {
              RUST_LOG = "info";
              # The canonical prod topology (secret-free, committable): the binary
              # reads env only (ev_lib::settings), and gitops' Deployment env
              # carries the same values — keep the two in step.
              CABINET_BACKEND_BIND = "0.0.0.0:50062";
              PIGGYBANK_GRPC_ADDR = "http://ev-banking-piggybank:50051";
              BANKING_AUTH_GRPC_ADDR = "http://ev-banking-piggybank:50052";
              CONCIERGE_GRPC_ADDR = "http://concierge:55670";
              # Mirror the concierge plane's (assert_plane-checked) identity values.
              AUTH_ISSUER = "https://auth.concierge.ev";
              AUTH_CLIENT_AUDIENCE = "concierge";
              MFE_REGISTRY_PATH = "/mfe-registry.json";
              # The PUBLIC origin the browser sends as `Origin` on the consilium websocket
              # handshake — not an internal service name. A ws handshake is exempt from CORS
              # and still carries cookies, so this is what stops a cross-site page opening the
              # live feed as the signed-in owner. It is `required_in("production")`, so a pod
              # without it refuses to boot rather than failing open: public topology, no
              # secret, which is why it belongs here and not in the cluster Secret.
              CABINET_WS_ORIGIN = "https://evinvest.ltd";
              APP_ENV = "production";
            };
          };
          containers.cabinet = {
            port = 50061;
            # basePath mount: the zone answers under /cabinet (3xx from / counts as probe success).
            healthPath = "/cabinet";
            criticality = "normal";
            entrypoint = [ "${pkgs.nodejs}/bin/node" "${cabinetApp}/cabinet/frontend/server.js" ];
            contents = [ pkgs.nodejs cabinetApp ];
            workingDir = "${cabinetApp}/cabinet/frontend";
            imageEnv = [ "PORT=50061" "HOSTNAME=0.0.0.0" "NODE_ENV=production" ];
            env = {
              # Runtime reads (proxy.ts CSP etc.) — the rewrite itself is baked at build.
              CABINET_BACKEND_URL = "http://ev-banking-cabinet-backend:50062";
              # concierge's web surface, dialled server-to-server by proxy.ts to renew a
              # lapsing ev_access before a server render forwards it to the BFF
              # (shared/lib/access-renewal.ts). Unset, renewal is silently off and the first
              # server read after a pause meets a dead token. Same value site_conductor's
              # frontend rewrites /api/auth/* to; the `web` port of the `concierge` Service in
              # ev-banking, whose ingress policy leaves :55671 open to every source.
              AUTH_WEB_URL = "http://concierge:55671";
            };
          };
        };

        # ── proto → OpenAPI → TS codegen plugin ─────────────────────────────
        # protoc-gen-connect-openapi as a pinned release binary (same approach as the
        # tigerbeetle/zig binaries above) — offline and supply-chain-pinned. It turns
        # the gRPC protos into an OpenAPI doc; @hey-api/openapi-ts then emits the
        # cabinet's TypeScript types. The proto stays the single wire source of truth.
        protocGenConnectOpenapi =
          let
            version = "0.25.7";
            dist = {
              x86_64-linux = { file = "protoc-gen-connect-openapi_${version}_linux_amd64.tar.gz"; hash = "sha256-3eqIe0Mt9Ucaxn72FncmtGpPM3rw7sT1+ImY+sX7bMI="; };
              aarch64-linux = { file = "protoc-gen-connect-openapi_${version}_linux_arm64.tar.gz"; hash = "sha256-1yKaOsZZdOAkIQUiyzkg52T930COJstQHrQUBuUU1uw="; };
              x86_64-darwin = { file = "protoc-gen-connect-openapi_${version}_darwin_all.tar.gz"; hash = "sha256-cXyGw8oDRkV7PkxfaVlrKLKy7GvwR0mAAcAr+tjln1I="; };
              aarch64-darwin = { file = "protoc-gen-connect-openapi_${version}_darwin_all.tar.gz"; hash = "sha256-cXyGw8oDRkV7PkxfaVlrKLKy7GvwR0mAAcAr+tjln1I="; };
            }.${system};
          in
          pkgs.stdenvNoCC.mkDerivation {
            pname = "protoc-gen-connect-openapi";
            inherit version;
            src = pkgs.fetchurl {
              url = "https://github.com/sudorandom/protoc-gen-connect-openapi/releases/download/v${version}/${dist.file}";
              inherit (dist) hash;
            };
            sourceRoot = ".";
            dontConfigure = true;
            dontBuild = true;
            dontFixup = true;
            installPhase = ''
              mkdir -p $out/bin
              install -m755 protoc-gen-connect-openapi $out/bin/
            '';
          };

        # ── piggybank (the hub server: core + auth, gRPC only) ──────────────
        # Runs `piggybank-core`, which spawns the core gRPC services and the auth
        # service as in-process tasks. A reachable Postgres + TigerBeetle are the
        # prerequisites (`.#db`/`.#tb`, or `.#dev`). No HTTP: browser traffic
        # reaches the hub through the `cabinet` BFF. Defaults mirror
        # piggybank/core/.env.example; any value already in the environment wins.
        runPiggybank = pkgs.writeShellApplication {
          name = "run-piggybank";
          runtimeInputs = with pkgs; [ rust pkg-config openssl protobuf git ];
          text = ''
            ${dyldFallback}
            ${protocEnv}
            repo="$(git rev-parse --show-toplevel)"
            cd "$repo"

            ${linkTbClient}

            # Load the hub's local config (signing key, Google OAuth creds, …) the way
            # dotenvy would — but from piggybank/core/.env, since we run from the repo
            # root (dotenvy only checks ./.env). `set -a` exports each assignment;
            # already-set env vars are not overwritten by `.env`, and the defaults
            # below still fill anything the file leaves unset.
            set -a
            if [ -f piggybank/core/.env ]; then
              # shellcheck disable=SC1091
              . piggybank/core/.env
            fi
            set +a

            ${portEnv}
            export DATABASE_URL="''${DATABASE_URL:-postgres://postgres@localhost:$POSTGRES_PORT/banking}"
            export GRPC_ADDR="''${GRPC_ADDR:-0.0.0.0:$PIGGYBANK_CORE_PORT}"
            export AUTH_GRPC_ADDR="''${AUTH_GRPC_ADDR:-0.0.0.0:$PIGGYBANK_AUTH_PORT}"
            export RUST_LOG="''${RUST_LOG:-info,piggybank_core=debug,evbanking_auth=debug}"
            # Central-only refresh-token store; harmless if unused (auth is scaffold).
            export REDIS_URL="''${REDIS_URL:-redis://127.0.0.1:$REDIS_PORT/0}"
            export TIGERBEETLE_ADDRESS="''${TIGERBEETLE_ADDRESS:-127.0.0.1:$TIGERBEETLE_PORT}"
            export TIGERBEETLE_CLUSTER_ID="''${TIGERBEETLE_CLUSTER_ID:-0}"
            export SIGNER_GRPC_ADDR="''${SIGNER_GRPC_ADDR:-http://127.0.0.1:$SIGNER_PORT}"
            # BSC_RPC_URL (set it in piggybank/core/.env) — free public endpoints all have
            # sharp edges for the on-chain workers: publicnode paywalls eth_getLogs, drpc's
            # free tier rate-limits the deposit scan away, and dataseed.bnbchain.org rejects
            # wide eth_getLogs ranges (fine once the scan cursor rides the chain head, fatal
            # for a cold-start backfill). For anything beyond a demo use a KEYED provider
            # (e.g. a free-tier Alchemy/QuickNode/Ankr key) so getLogs + balance polling
            # survive a real scan cycle.
            # POLYGON_RPC_URL (also piggybank/core/.env) — the second EVM rail, same eth_getLogs
            # caveat. Bring it up on Amoy testnet first: POLYGON_RPC_URL=<amoy endpoint>,
            # POLYGON_CHAIN_ID=80002, POLYGON_USDT_CONTRACT=<your test ERC20>. A testnet endpoint
            # needs its matching POLYGON_CHAIN_ID (80002) — the default is mainnet 137, and a hub
            # that mixes a testnet rail with a mainnet one refuses to boot (claims are network-
            # agnostic, so a testnet deposit would withdraw as real USDT). Mainnet defaults
            # (chain 137, native USDT 0xc213…58e8F @ 6-dp, 128 confs) need no override. Flip Amoy→
            # mainnet on a FRESH database — the rail's state is keyed by network, not chain, so an
            # in-place re-point is refused by the rail_chain_identity guard. Keep the fund-moving
            # sweep off (POLYGON_SWEEP_ENABLED unset) until the treasury + gas station are funded
            # with USDT + POL — see docs/RUNBOOK-polygon-rail.md.
            # Cross-plane lifecycle bridge consumer (one-way concierge → banking). Both vars
            # must be set together or the consumer doesn't run; BRIDGE_SERVICE_TOKEN must match
            # the concierge plane's value. The concierge plane serves UserEvents on :50061.
            export CONCIERGE_BRIDGE_ADDR="''${CONCIERGE_BRIDGE_ADDR:-http://127.0.0.1:$CONCIERGE_PORT}"
            export BRIDGE_SERVICE_TOKEN="''${BRIDGE_SERVICE_TOKEN:-dev-bridge-token}"
            # Concierge→banking token-exchange seam: the BFF presents this on IssueUserToken to
            # mint the money-plane pair. Must match the cabinet-backend value below.
            export BANKING_ISSUANCE_TOKEN="''${BANKING_ISSUANCE_TOKEN:-dev-issuance-token}"
            # LiveSettings has no Rust-side default for app_env (String field);
            # dev topology is owned here, prod literals live in the piggybank container env (containerStd).
            export APP_ENV="''${APP_ENV:-development}"
            exec cargo run -p piggybank-core
          '';
        };

        # ── signer (the separate-process key vault) ─────────────────────────
        # Runs `piggybank-signer`: generates chain keypairs, stores the private keys
        # encrypted at rest in its OWN database, and serves key provisioning over gRPC.
        # A reachable Postgres is the only prerequisite (`.#db`, or `.#dev`). Defaults
        # mirror piggybank/signer/.env.example; any value already set wins.
        runSigner = pkgs.writeShellApplication {
          name = "run-signer";
          runtimeInputs = with pkgs; [ rust pkg-config openssl protobuf git ];
          text = ''
            ${dyldFallback}
            ${protocEnv}
            repo="$(git rev-parse --show-toplevel)"
            cd "$repo"

            ${linkTbClient}

            set -a
            if [ -f piggybank/signer/.env ]; then
              # shellcheck disable=SC1091
              . piggybank/signer/.env
            fi
            set +a

            ${portEnv}
            export SIGNER_DATABASE_URL="''${SIGNER_DATABASE_URL:-postgres://postgres@localhost:$POSTGRES_PORT/banking_signer}"
            # Loopback: the seam is authenticated (service JWT) but a wider bind
            # also requires TLS (SIGNER_TLS_*). The hub↔signer seam is single-host in dev.
            export SIGNER_GRPC_ADDR="''${SIGNER_GRPC_ADDR:-127.0.0.1:$SIGNER_PORT}"
            # The signer verifies the hub's service token against the auth service's JWKS.
            export AUTH_JWKS_GRPC_ENDPOINT="''${AUTH_JWKS_GRPC_ENDPOINT:-http://127.0.0.1:$PIGGYBANK_AUTH_PORT}"
            # Dev-only KEK: ephemeral per boot when unset, so no key bytes live in the
            # repo. Production MUST inject a STABLE 32-byte KEK from a secrets store/KMS
            # (a rotating KEK can't open previously-sealed keys). Set WALLET_KEK in
            # piggybank/signer/.env to keep sealed keys openable across restarts.
            export WALLET_KEK="''${WALLET_KEK:-$(openssl rand -hex 32)}"
            export RUST_LOG="''${RUST_LOG:-info,piggybank_signer=debug}"
            exec cargo run -p piggybank-signer
          '';
        };

        # ── cabinet backend (the BFF) ───────────────────────────────────────
        # Runs `cabinet-backend`: the cabinet's stateless HTTP BFF. It holds the session
        # + runs the OAuth flow and proxies the browser's /api/* to the piggybank money
        # plane (:50051) and the concierge identity plane (:50061 — run from the sibling
        # `concierge` repo). `linkTbClient` is required only so the Cargo workspace (which
        # contains the TB-using piggybank crates) resolves; the BFF itself never uses TB.
        # Defaults mirror cabinet/backend/.env.example; any value already set wins.
        runCabinetBackend = pkgs.writeShellApplication {
          name = "run-cabinet-backend";
          runtimeInputs = with pkgs; [ rust pkg-config openssl protobuf git ];
          text = ''
            ${dyldFallback}
            ${protocEnv}
            repo="$(git rev-parse --show-toplevel)"
            cd "$repo"

            ${linkTbClient}

            set -a
            if [ -f cabinet/backend/.env ]; then
              # shellcheck disable=SC1091
              . cabinet/backend/.env
            fi
            set +a

            ${portEnv}
            # Env aliases mirror AppConfig field names (LiveSettings `use_env`);
            # dev topology is owned here, prod literals live in the cabinet-backend container env (containerStd).
            export CABINET_BACKEND_BIND="''${CABINET_BACKEND_BIND:-0.0.0.0:$CABINET_BACKEND_PORT}"
            export PIGGYBANK_GRPC_ADDR="''${PIGGYBANK_GRPC_ADDR:-http://127.0.0.1:$PIGGYBANK_CORE_PORT}"
            export BANKING_AUTH_GRPC_ADDR="''${BANKING_AUTH_GRPC_ADDR:-http://127.0.0.1:$PIGGYBANK_AUTH_PORT}"
            # Money-plane token-exchange seam — must match the piggybank hub's BANKING_ISSUANCE_TOKEN.
            export BANKING_ISSUANCE_TOKEN="''${BANKING_ISSUANCE_TOKEN:-dev-issuance-token}"
            export CONCIERGE_GRPC_ADDR="''${CONCIERGE_GRPC_ADDR:-http://127.0.0.1:$CONCIERGE_PORT}"
            export AUTH_ISSUER="''${AUTH_ISSUER:-https://auth.concierge.ev}"
            export AUTH_CLIENT_AUDIENCE="''${AUTH_CLIENT_AUDIENCE:-concierge}"
            export MFE_REGISTRY_PATH="''${MFE_REGISTRY_PATH:-cabinet/frontend/mfe-registry.json}"
            export APP_ENV="''${APP_ENV:-development}"
            export RUST_LOG="''${RUST_LOG:-info,cabinet_backend=debug}"
            exec cargo run -p cabinet-backend
          '';
        };

        # ── clients (Turborepo: Next.js host) ───────────────────────────────
        # npm workspaces rooted at the repo; deps install once into the hoisted
        # node_modules (`npm install` also generates/updates the lockfile). The `cabinet`
        # frontend proxies /api/* to the cabinet backend (BFF) over HTTP (same-origin).
        runCabinet = pkgs.writeShellApplication {
          name = "run-cabinet";
          runtimeInputs = with pkgs; [ nodejs git ];
          text = ''
            repo="$(git rev-parse --show-toplevel)"
            cd "$repo"
            [ -d node_modules/next ] || npm install
            ${portEnv}
            export CABINET_BACKEND_URL="''${CABINET_BACKEND_URL:-http://127.0.0.1:$CABINET_BACKEND_PORT}"
            exec npm run dev --workspace @evbanking/cabinet -- --port "$CABINET_FRONTEND_PORT"
          '';
        };

        # ── contracts codegen (proto → OpenAPI → cabinet TS types) ──────────
        # `nix run .#gen-api` regenerates `contracts/openapi.json` from the protos and
        # the cabinet's `shared/contracts/gen` types from that. Run it after editing a
        # proto; the outputs are committed so the app builds without the toolchain.
        #
        # The cabinet's identity surface (profile + sessions) is served by the concierge
        # plane, so its TS types come from concierge's OWN proto — not banking's copy —
        # sourced from the pinned `evconcierge_contracts` git dep that the BFF already
        # compiles against (resolved via `cargo metadata`, so it tracks Cargo.lock's pin).
        # Both protos feed one merged OpenAPI doc; service FQNs keep paths/schemas
        # namespaced (`banking.v1.*` vs `concierge.v1.*`), so the gen emits both
        # `BankingV1*` and `ConciergeV1*` types with no collision.
        #
        # WHICH concierge protos: contracts/concierge-protos.txt — the same list
        # `concierge-pin-check` guards, so a proto cannot be generated from without
        # its bytes being pinned (#293: governance.proto was fed here but not checked).
        runGenApi = pkgs.writeShellApplication {
          name = "run-gen-api";
          runtimeInputs = with pkgs; [ protobuf protocGenConnectOpenapi nodejs git cargo jq ];
          text = ''
            repo="$(git rev-parse --show-toplevel)"
            cd "$repo"
            cc_dir="$(dirname "$(cargo metadata --format-version 1 --manifest-path contracts/Cargo.toml \
              | jq -r '.packages[] | select(.name=="evconcierge_contracts") | .manifest_path')")"
            mapfile -t cc_protos < <(grep -Ev '^[[:space:]]*(#|$)' contracts/concierge-protos.txt)
            echo "▶ proto (banking + concierge: ''${cc_protos[*]}) → contracts/openapi.json"
            protoc -I contracts/proto -I "$cc_dir/proto" \
              --connect-openapi_out=contracts \
              --connect-openapi_opt=format=json,path=openapi.json,with-proto-names \
              contracts/proto/banking/v1/*.proto \
              "''${cc_protos[@]/#/$cc_dir/proto/}"
            echo "▶ openapi.json → cabinet TypeScript types"
            [ -d node_modules ] || npm install
            npm run gen:api --workspace @evbanking/cabinet
            echo "✓ regenerated contracts/openapi.json + cabinet/shared/contracts/gen"
          '';
        };

        # ── cross-repo concierge pin guard ──────────────────────────────────
        # `nix run .#concierge-pin-check` — assert the pinned `evconcierge_contracts`
        # rev (the identity + governance wire contract banking compiles and re-aliases
        # its cabinet TS from) is an ancestor of concierge origin/main, and that every
        # proto in contracts/concierge-protos.txt has the same bytes at the pin, on
        # origin/main and in the local cargo checkout gen-api reads. CI entry point for
        # the contract-parity guard (drift.yml); needs network to the remote.
        #
        # `rust` + the tb-client link + empty RUSTC_WRAPPER as in runDriftCheck below:
        # the checkout is located via `cargo metadata`, which needs the workspace to
        # resolve (the .tb-client path dep) and no sccache probe (.cargo/config.toml).
        # `bash` explicitly: the script needs bash ≥ 4 (`mapfile`), and `exec bash`
        # would otherwise pick up macOS's system 3.2 from PATH.
        runConciergePinCheck = pkgs.writeShellApplication {
          name = "run-concierge-pin-check";
          runtimeInputs = with pkgs; [ bash rust git jq gnused coreutils gnugrep ];
          text = ''
            cd "$(git rev-parse --show-toplevel)"
            ${linkTbClient}
            export RUSTC_WRAPPER=""
            exec bash contracts/concierge-pin-check.sh
          '';
        };

        # ── drift gate (CI entry point) ─────────────────────────────────────
        # `nix run .#drift-check` — the two committed artefacts that are OUTPUT, and
        # so can disagree with the source they were generated from without anything
        # failing to build:
        #
        #   Cargo.lock vs the manifests. `--locked` refuses to re-resolve, so a
        #   textual merge that took a new git rev in Cargo.toml while keeping the old
        #   one in Cargo.lock exits 101 HERE. v0.3.6 found it at the release image
        #   build instead: a dev-shell `cargo test` has network, so cargo quietly
        #   re-resolved and rewrote the lock in the working tree, and the tests went
        #   green against a file nobody committed.
        #
        #   contracts/openapi.json + the cabinet's TS types vs the protos. Both are
        #   committed so the app builds without the codegen toolchain — which also
        #   means a stale pair ships and is discovered by whoever first wants a new
        #   field.
        #
        # `rust`, not `pkgs.cargo`: this workspace sets an unstable `codegen-backend`
        # in .cargo/config.toml, and stable cargo refuses to parse the manifest at all
        # — with exit 101, the SAME code a genuine lock mismatch gives. A gate built on
        # stable would be red on a clean tree and indistinguishable from a real find.
        runDriftCheck = pkgs.writeShellApplication {
          name = "run-drift-check";
          runtimeInputs = [ rust pkgs.git pkgs.nodejs runGenApi ];
          text = ''
            cd "$(git rev-parse --show-toplevel)"

            # A fresh clone — which is what CI is — has no .tb-client, and the workspace
            # carries a path dependency on it, so `cargo metadata` cannot resolve without
            # this. Same reason every other workspace-touching app links it first.
            ${linkTbClient}

            # .cargo/config.toml sets `rustc-wrapper = "sccache"`, so cargo probes
            # `sccache rustc -vV` before doing anything and dies with 101 wherever sccache
            # is not on PATH — every runner. An empty RUSTC_WRAPPER overrides the config
            # key; it is the same override the generated workflows carry in their `env`.
            # Nothing is compiled here anyway: `metadata` resolves, it does not build.
            export RUSTC_WRAPPER=""

            echo "▶ Cargo.lock agrees with the manifests"
            cargo metadata --locked --format-version 1 >/dev/null

            echo "▶ the committed contract is what the protos generate"
            generated=(contracts/openapi.json cabinet/frontend/shared/contracts/gen)
            # Ahead of gen-api's own `npm install` fallback, and deliberately `ci`: on a
            # fresh checkout `install` is free to move a generator inside its semver range,
            # which would show up here as contract drift nobody caused.
            [ -d node_modules ] || npm ci
            run-gen-api
            # --porcelain rather than `git diff --exit-code`: a generator that emits a
            # NEW file leaves it untracked, and a diff alone calls that clean.
            drift="$(git status --porcelain -- "''${generated[@]}")"
            if [ -n "$drift" ]; then
              echo "::error::the committed contract is stale — run 'nix run .#gen-api' and commit both sides" >&2
              echo "$drift" >&2
              git --no-pager diff -- "''${generated[@]}" >&2
              exit 1
            fi

            echo "✓ no drift"
          '';
        };

        # ── the build/test gate (CI entry points) ───────────────────────────
        # Until these, nothing in CI ever compiled this repository. `jobs` below asks
        # for `tokei`, `code-duplication` and `loc-badge` and nothing else, so no
        # errors.yml was generated at all and a green tick on a PR said only that the
        # comment ratio was acceptable.
        #
        # The generator's own `rust-tests` / `rust-clippy` cannot be turned on here,
        # and not for a reason a flag fixes: they drive cargo from a
        # dtolnay/rust-toolchain runner, while this workspace declares
        # `tigerbeetle = { path = ".tb-client" }` — a symlink into the nix store that
        # only the dev shell creates. Outside nix there is no such directory and cargo
        # cannot resolve the workspace at all. Hence a nix entry point, like the drift
        # gate above and the container release: inside the flake the toolchain, protoc
        # and .tb-client all exist and match what developers run.
        runCheckRust = pkgs.writeShellApplication {
          name = "run-check-rust";
          runtimeInputs = (with pkgs; [ rust git protobuf mold postgresql coreutils ]) ++ [ tigerbeetleBin ]; # tests/ledger_gap.rs runs its own clusters
          text = ''
            cd "$(git rev-parse --show-toplevel)"
            ${linkTbClient}
            ${protocEnv}
            # See runDriftCheck: .cargo/config.toml names sccache as the rustc wrapper,
            # which is not on a runner's PATH and aborts cargo before it starts.
            export RUSTC_WRAPPER=""

            echo "▶ clippy (workspace, all targets, warnings denied)"
            cargo clippy --workspace --all-targets -- -D warnings

            # Until #259 this step ran deliberately WITHOUT DATABASE_URL, and every suite
            # under piggybank/*/tests skipped itself — four releases went out on a green
            # tick that had never touched Postgres or TigerBeetle. Now the gate brings up
            # both from the same flake apps developers run (`.#db`, `.#tb`; the TB binary
            # is the 0.17.6 the client is built against), in a throwaway state dir on
            # ports away from the shared dev cluster, and sets CI=true so
            # tests/common/mod.rs FAILS instead of skipping when a service is missing.
            # /tmp rather than mktemp's default: macOS's $TMPDIR pushes the postgres
            # socket path past the 103-byte sun_path limit.
            scratch="$(mktemp -d /tmp/check-rust.XXXXXX)"
            export XDG_STATE_HOME="$scratch/state"
            export TB_DATA="$scratch/tb"
            export POSTGRES_PORT="''${CHECK_POSTGRES_PORT:-54329}"
            export TIGERBEETLE_PORT="''${CHECK_TIGERBEETLE_PORT:-3039}"
            export PGDATABASES="banking_check"
            tb_pid=""
            cleanup() {
              if [ -n "$tb_pid" ]; then kill "$tb_pid" 2>/dev/null || true; fi
              pg_ctl -D "$XDG_STATE_HOME/ev_invest/pg/data" -m fast stop >/dev/null 2>&1 || true
              rm -rf "$scratch"
            }
            trap cleanup EXIT

            echo "▶ postgres :$POSTGRES_PORT + tigerbeetle :$TIGERBEETLE_PORT (throwaway)"
            ${runPostgres}/bin/run-postgres
            TBCLUSTER=0 ${runTigerbeetle}/bin/run-tigerbeetle >"$scratch/tb.log" 2>&1 &
            tb_pid=$!
            for _ in $(seq 1 150); do
              if (exec 3<>"/dev/tcp/127.0.0.1/$TIGERBEETLE_PORT") 2>/dev/null; then break; fi
              if ! kill -0 "$tb_pid" 2>/dev/null; then cat "$scratch/tb.log" >&2; echo "tigerbeetle exited" >&2; exit 1; fi
              sleep 0.2
            done
            if ! (exec 3<>"/dev/tcp/127.0.0.1/$TIGERBEETLE_PORT") 2>/dev/null; then
              cat "$scratch/tb.log" >&2; echo "tigerbeetle did not come up on :$TIGERBEETLE_PORT" >&2; exit 1
            fi
            export DATABASE_URL="postgres://postgres@127.0.0.1:$POSTGRES_PORT/banking_check"
            # The signer suites create their own throwaway databases from this URL.
            export SIGNER_DATABASE_URL="$DATABASE_URL"
            export TIGERBEETLE_ADDRESS="127.0.0.1:$TIGERBEETLE_PORT"
            export TIGERBEETLE_CLUSTER_ID=0
            export CI=true

            echo "▶ tests (workspace, integration suites against the throwaway services)"
            cargo test --workspace

            echo "✓ rust ok"
          '';
        };

        runCheckFrontend = pkgs.writeShellApplication {
          name = "run-check-frontend";
          runtimeInputs = with pkgs; [ nodejs git ];
          text = ''
            cd "$(git rev-parse --show-toplevel)"
            # `ci`, not `install`: the lock is the point of a check run.
            npm ci

            echo "▶ tsc --noEmit"
            npm run check --workspace @evbanking/cabinet

            echo "▶ cabinet tests"
            npm run test --workspace @evbanking/cabinet

            echo "▶ cabinet i18n catalogues"
            npm run i18n:check --workspace @evbanking/cabinet

            # `npm run lint` is deliberately absent: it reports 5 errors on main today
            # (profile-schema.ts, instrumentation.ts, next.config.ts, profile-view.tsx,
            # settings/ui/fields.tsx). Adding it now would land red, which is the exact
            # thing this gate exists to stop being normal. Clean those, then add it.

            echo "✓ frontend ok"
          '';
        };

        # ── shared Redis (ensure-running) ───────────────────────────────────
        # ONE instance for all ev_invest repos (numeric dbs: 0=banking, 1=concierge),
        # daemonized under the user state dir so no repo's dev-stack exit can yank it
        # out from under the siblings. Stop: redis-cli -p $REDIS_PORT shutdown nosave
        runRedis = pkgs.writeShellApplication {
          name = "run-redis";
          runtimeInputs = with pkgs; [ redis coreutils ];
          text = ''
            ${portEnv}
            state="''${XDG_STATE_HOME:-$HOME/.local/state}/ev_invest"
            mkdir -p "$state/redis"
            if ! redis-cli -p "$REDIS_PORT" ping >/dev/null 2>&1; then
              # AOF on: auth sessions + refresh families live here, and without
              # persistence every redis restart signs everyone out.
              redis-server --port "$REDIS_PORT" --dir "$state/redis" --save "" --appendonly yes \
                --daemonize yes --logfile "$state/redis/log"
            fi
            echo "redis ready on 127.0.0.1:$REDIS_PORT"
          '';
        };

        # ── shared Postgres (ensure-running) ────────────────────────────────
        # ONE trust-auth cluster for all ev_invest repos, under the user state dir —
        # NOT the repo. Started detached (same reasoning as redis above); each repo's
        # runner only ensures its own databases exist (database name == app name).
        # Stop: pg_ctl -D ~/.local/state/ev_invest/pg/data stop
        runPostgres = pkgs.writeShellApplication {
          name = "run-postgres";
          runtimeInputs = with pkgs; [ postgresql coreutils gnugrep util-linux ];
          text = ''
            ${portEnv}
            state="''${XDG_STATE_HOME:-$HOME/.local/state}/ev_invest"
            export PGDATA="$state/pg/data"
            sockets="$state/pg/sockets"
            dbs="''${PGDATABASES:-banking banking_signer}"
            mkdir -p "$sockets"

            # Serialize sibling repos racing to first-boot the shared cluster.
            exec 9>"$state/pg.lock"
            flock 9

            if ! pg_isready --host="$sockets" --port="$POSTGRES_PORT" --quiet; then
              # TCP answering while our socket is silent = some OTHER cluster owns
              # the port — refuse rather than silently use the wrong database.
              if pg_isready --host=127.0.0.1 --port="$POSTGRES_PORT" --quiet; then
                echo "error: 127.0.0.1:$POSTGRES_PORT serves a postgres that is not the shared ev_invest cluster" >&2
                exit 1
              fi
              if [ ! -s "$PGDATA/PG_VERSION" ]; then
                echo "initialising shared postgres cluster in $PGDATA"
                initdb --username=postgres --auth=trust --pgdata="$PGDATA" >/dev/null
              fi
              chmod 0700 "$PGDATA"
              # 9>&- : the daemon must NOT inherit the lock fd, or it holds the
              # flock for its lifetime and every later ensure-run blocks forever.
              pg_ctl -D "$PGDATA" -l "$state/pg/log" -o "-k $sockets -h 127.0.0.1 -p $POSTGRES_PORT" start 9>&-
            fi
            exec 9>&-

            for db in $dbs; do
              if ! psql --host="$sockets" --port="$POSTGRES_PORT" --username=postgres --dbname=postgres \
                     --tuples-only --no-align \
                     --command "SELECT 1 FROM pg_database WHERE datname='$db'" | grep -q 1; then
                createdb --host="$sockets" --port="$POSTGRES_PORT" --username=postgres "$db"
                echo "created database '$db'"
              fi
            done
            echo "postgres ready on 127.0.0.1:$POSTGRES_PORT (databases ensured: $dbs)"
          '';
        };

        # ── local TigerBeetle ───────────────────────────────────────────────
        # Project-local ledger under .tb/ (gitignored). First run formats a
        # single-replica cluster; later runs just start it. Port 3033 keeps the
        # ledger off the 3000 web range owned by `cabinet`.
        runTigerbeetle = pkgs.writeShellApplication {
          name = "run-tigerbeetle";
          runtimeInputs = [ tigerbeetleBin pkgs.git ];
          text = ''
            ${portEnv}
            repo="$(git rev-parse --show-toplevel)"
            # Overridable so a gate can point a throwaway replica away from the dev ledger.
            export TB_DATA="''${TB_DATA:-$repo/.tb/data}"
            port="$TIGERBEETLE_PORT"
            cluster_id="''${TBCLUSTER:-0}"
            data_file="$TB_DATA/''${cluster_id}_0.tigerbeetle"

            mkdir -p "$TB_DATA"
            if [ ! -f "$data_file" ]; then
              echo "formatting TigerBeetle data file (cluster=''${cluster_id}, replica=0, replica-count=1)"
              tigerbeetle format --cluster="$cluster_id" --replica=0 --replica-count=1 "$data_file"
            fi

            echo "TigerBeetle ready on 127.0.0.1:$port (cluster ''${cluster_id})"
            exec tigerbeetle start --addresses="127.0.0.1:$port" "$data_file"
          '';
        };

        # ── prod replica move: rpi5 → VPS ───────────────────────────────────
        # `nix run .#new-replica -- --host <ssh> --replica <i>` — moves replica i
        # of the PROD cluster (deploy/tigerbeetle.nix) onto a possibly non-NixOS
        # host. Rebuilds the data file with `tigerbeetle recover` — NEVER format:
        # a re-formatted replica false-nacks prepares it already acknowledged and
        # can destroy committed data. Preconditions assert the runbook order
        # (deploy/tigerbeetle.nix edited → ~/nix mirror rebuilt → this app), then:
        # ship the arch-matched static binary, recover from the survivors, install
        # a plain systemd unit, and rolling-restart the rpi5 survivors one at a
        # time (quorum = 2 of 3 — never two replicas down at once).
        runNewReplica =
          let
            deploy = import ./deploy/tigerbeetle.nix;
            n = builtins.length deploy.addresses;
            portOf = a: pkgs.lib.last (pkgs.lib.splitString ":" a);
            # Replica i binds any-address on its own port, dials peers at their
            # canonical addresses — same shape the rpi5 units use.
            bindCsv = i: pkgs.lib.concatStringsSep ","
              (pkgs.lib.imap0 (j: a: if j == i then "0.0.0.0:${portOf a}" else a) deploy.addresses);
          in
          pkgs.writeShellApplication {
            name = "new-replica";
            runtimeInputs = with pkgs; [ openssh coreutils ];
            # SC2029/SC2087: every `ssh "$host" …` command and heredoc here
            # expands client-side on purpose (the remote gets final values).
            excludeShellChecks = [ "SC2029" "SC2087" ];
            text = ''
              usage() { echo "usage: new-replica --host <ssh-target, root-capable> --replica <i> [--rpi5 <ssh>]" >&2; exit 1; }
              host="" replica="" rpi5="rpi5-ts"
              while [ $# -gt 0 ]; do
                case "$1" in
                  --host) host="$2"; shift 2 ;;
                  --replica) replica="$2"; shift 2 ;;
                  --rpi5) rpi5="$2"; shift 2 ;;
                  *) usage ;;
                esac
              done
              [ -n "$host" ] && [ -n "$replica" ] || usage
              [ "$replica" -ge 0 ] && [ "$replica" -lt ${toString n} ] || { echo "replica must be 0..$((${toString n} - 1))" >&2; exit 1; }

              cluster_id=${deploy.clusterId}
              csv=${pkgs.lib.concatStringsSep "," deploy.addresses}
              bind_csv=(${pkgs.lib.concatMapStringsSep " " (i: ''"${bindCsv i}"'') (pkgs.lib.genList (x: x) n)})
              data_file="/var/lib/tigerbeetle/''${cluster_id}_''${replica}.tigerbeetle"

              runbook() {
                cat >&2 <<EOF
              aborting — required order for a replica move:
                1. edit banking/deploy/tigerbeetle.nix: point addresses[$replica] at the new host
                2. mirror in ~/nix hosts/rpi5/tigerbeetle.nix (addresses + drop $replica from localReplicas), rebuild rpi5
                3. re-run: nix run .#new-replica -- --host $host --replica $replica
              EOF
                exit 1
              }

              # Replica i's data must have exactly one live home; an active unit on
              # rpi5 means step 2 (drop from localReplicas + rebuild) didn't happen.
              state=$(ssh "$rpi5" systemctl is-active "tigerbeetle-$replica.service" 2>/dev/null || true)
              if [ "$state" = "active" ] || [ "$state" = "activating" ]; then
                echo "tigerbeetle-$replica is still $state on $rpi5" >&2
                runbook
              fi

              # Survivors must already be DEPLOYED with the current address list
              # (their running processes are rolling-restarted at the end).
              for j in $(seq 0 $((${toString n} - 1))); do
                [ "$j" = "$replica" ] && continue
                if ! unit=$(ssh "$rpi5" systemctl cat "tigerbeetle-$j.service" 2>/dev/null); then
                  echo "note: tigerbeetle-$j not on $rpi5 (moved earlier?) — skipping its ExecStart check"
                  continue
                fi
                expected="--addresses=''${bind_csv[$j]}"
                case "$unit" in
                  *"$expected"*) ;;
                  *)
                    echo "tigerbeetle-$j on $rpi5 does not carry $expected — the ~/nix mirror wasn't updated/rebuilt" >&2
                    runbook
                    ;;
                esac
              done

              arch=$(ssh "$host" uname -m)
              case "$arch" in
                x86_64) bin="${tigerbeetleBinFor "x86_64-linux"}/bin/tigerbeetle" ;;
                aarch64) bin="${tigerbeetleBinFor "aarch64-linux"}/bin/tigerbeetle" ;;
                *) echo "unsupported target arch: $arch" >&2; exit 1 ;;
              esac
              echo "▶ shipping tigerbeetle 0.17.6 ($arch) to $host"
              scp "$bin" "$host:/tmp/tigerbeetle-0.17.6"
              ssh "$host" "install -m755 /tmp/tigerbeetle-0.17.6 /usr/local/bin/tigerbeetle && rm /tmp/tigerbeetle-0.17.6"

              # A leftover data file is exactly the stale-replica amnesia hazard —
              # never recover over it, never reuse it.
              ssh "$host" "mkdir -p /var/lib/tigerbeetle && test ! -e $data_file" \
                || { echo "$data_file already exists on $host — refusing to touch it" >&2; exit 1; }
              echo "▶ recovering replica $replica from the survivors (this streams the full data file)"
              ssh "$host" "/usr/local/bin/tigerbeetle recover --cluster=$cluster_id --addresses=$csv --replica=$replica --replica-count=${toString n} $data_file"

              echo "▶ installing tigerbeetle-$replica.service on $host"
              ssh "$host" "cat > /etc/systemd/system/tigerbeetle-$replica.service" <<EOF
              [Unit]
              Description=TigerBeetle replica $replica
              After=network-online.target

              [Service]
              ExecStart=/usr/local/bin/tigerbeetle start --addresses=''${bind_csv[$replica]} $data_file
              Restart=always
              RestartSec=5s

              [Install]
              WantedBy=multi-user.target
              EOF
              ssh "$host" "systemctl daemon-reload && systemctl enable --now tigerbeetle-$replica.service"

              # Survivors still dial replica $replica at its old address until
              # restarted; one at a time, waiting for is-active, so quorum holds.
              for j in $(seq 0 $((${toString n} - 1))); do
                [ "$j" = "$replica" ] && continue
                ssh "$rpi5" systemctl cat "tigerbeetle-$j.service" >/dev/null 2>&1 || continue
                echo "▶ rolling restart: tigerbeetle-$j on $rpi5"
                ssh "$rpi5" "printf ' ' | sudo -S systemctl restart tigerbeetle-$j.service"
                ok=""
                for _ in $(seq 1 30); do
                  if [ "$(ssh "$rpi5" systemctl is-active "tigerbeetle-$j.service" 2>/dev/null || true)" = "active" ]; then ok=1; break; fi
                  sleep 2
                done
                [ -n "$ok" ] || { echo "tigerbeetle-$j did not come back on $rpi5 — stopping the rollout (never two replicas down)" >&2; exit 1; }
              done
              echo "✓ replica $replica now serves from $host"
            '';
          };

        # ── one-shot env init ───────────────────────────────────────────────
        # `nix run .#init` — everything a fresh clone needs beyond what the run
        # scripts already self-provision lazily (postgres cluster, TB data file,
        # databases): generated dev secrets in the gitignored .env files, npm deps,
        # and the TB client link. Idempotent: existing .env files are left alone.
        runInit = pkgs.writeShellApplication {
          name = "run-init";
          runtimeInputs = with pkgs; [ git openssl nodejs coreutils ];
          text = ''
            repo="$(git rev-parse --show-toplevel)"
            cd "$repo"

            ${linkTbClient}
            echo '✓ .tb-client link'

            # A stable KEK: the flake's per-boot random default can't reopen keys
            # sealed on a previous boot (the signer's KEK epoch guard refuses).
            if [ ! -f piggybank/signer/.env ]; then
              {
                echo '# generated by nix run .#init — dev-only secrets, gitignored'
                echo "WALLET_KEK=$(openssl rand -hex 32)"
              } > piggybank/signer/.env
              echo '✓ piggybank/signer/.env (persistent dev WALLET_KEK)'
            else
              echo '· piggybank/signer/.env exists, leaving as is'
            fi

            # Without a signing key the auth service boots inert (no token issuance,
            # so no money routes in the cabinet) — generate a dev Ed25519 keypair.
            if [ ! -f piggybank/core/.env ]; then
              key="$(openssl genpkey -algorithm ed25519)"
              x="$(printf '%s\n' "$key" | openssl pkey -pubout -outform DER | tail -c 32 | basenc --base64url | tr -d '=')"
              {
                echo '# generated by nix run .#init — dev-only secrets, gitignored'
                printf 'AUTH_SIGNING_KEY_PEM="%s"\n' "$key"
                echo 'AUTH_SIGNING_KID=local-dev'
                printf 'AUTH_JWKS_JSON=%s{"keys":[{"kty":"OKP","crv":"Ed25519","kid":"local-dev","x":"%s","alg":"EdDSA","use":"sig"}]}%s\n' "'" "$x" "'"
              } > piggybank/core/.env
              echo '✓ piggybank/core/.env (dev Ed25519 signing key + JWKS)'
            else
              echo '· piggybank/core/.env exists, leaving as is'
            fi

            [ -d node_modules/next ] || npm install
            echo '✓ npm deps'

            echo 'init done — nix run .#dev brings the stack up'
          '';
        };

        # ── full dev orchestrator ───────────────────────────────────────────
        # `nix run .#dev` → ensures the SHARED postgres + redis (detached, survive
        # this stack), then owns TigerBeetle + signer + piggybank + cabinet-backend +
        # cabinet; a single trap tears the owned tree down on exit. (Concierge, the
        # identity plane, lives in its own repo — start it there.)
        runDev = pkgs.writeShellApplication {
          name = "run-dev";
          runtimeInputs = with pkgs; [ coreutils ];
          text = ''
            ${portEnv}
            pids=()
            cleanup() {
              echo; echo "shutting down dev stack…"
              [ ''${#pids[@]} -gt 0 ] && kill "''${pids[@]}" 2>/dev/null || true
              wait 2>/dev/null || true
            }
            trap cleanup EXIT INT TERM

            echo "▶ postgres (shared)"
            ${runPostgres}/bin/run-postgres
            echo "▶ redis (shared)"
            ${runRedis}/bin/run-redis

            echo "▶ tigerbeetle (:$TIGERBEETLE_PORT)"
            ${runTigerbeetle}/bin/run-tigerbeetle & pids+=($!)

            echo "▶ signer    (:$SIGNER_PORT)"
            ${runSigner}/bin/run-signer & pids+=($!)
            echo "▶ piggybank (:$PIGGYBANK_CORE_PORT core / :$PIGGYBANK_AUTH_PORT auth)"
            ${runPiggybank}/bin/run-piggybank & pids+=($!)
            echo "▶ cabinet-backend (:$CABINET_BACKEND_PORT, BFF)"
            ${runCabinetBackend}/bin/run-cabinet-backend & pids+=($!)
            echo "▶ cabinet   (:$CABINET_FRONTEND_PORT)"
            ${runCabinet}/bin/run-cabinet & pids+=($!)

            wait
          '';
        };

        # ── bump latest remote vX.Y.Z tag and push: `.#publish major|minor|patch` ──
        runPublish = pkgs.writeShellApplication {
          name = "publish";
          runtimeInputs = with pkgs; [ git ];
          text = ''
                        part="''${1:-}"
                        case "$part" in major|minor|patch) ;; *) echo "usage: nix run .#publish -- major|minor|patch" >&2; exit 1 ;; esac
                        [ -z "$(git status --porcelain)" ] || { echo "uncommitted changes — commit or stash first" >&2; exit 1; }

                        git fetch --tags --force origin >/dev/null 2>&1
                        last="$(git tag -l 'v*' --sort=-v:refname | head -n1)"
                        ver="''${last#v}"; [ -n "$ver" ] || ver="0.0.0"
                        IFS=. read -r ma mi pa <<EOF
            $ver
            EOF
                        case "$part" in
                          major) ma=$((ma+1)); mi=0; pa=0 ;;
                          minor) mi=$((mi+1)); pa=0 ;;
                          patch) pa=$((pa+1)) ;;
                        esac
                        next="v$ma.$mi.$pa"
                        echo "$last → $next"
                        git tag "$next"
                        git push origin "$next"
          '';
        };
      in
      {
        # `nix run .#init`           → one-shot env setup for a fresh clone (dev .env secrets, npm deps, TB client link)
        # `nix run .#dev`            → everything (postgres + tigerbeetle + redis + signer + piggybank + cabinet-backend + cabinet)
        # `nix run .#piggybank`      → hub server: core gRPC + auth tasks (applies DB migrations on boot; needs DB + TB + signer: `.#db`/`.#tb`/`.#signer`, or `.#dev`)
        # `nix run .#signer`         → key vault: generates+seals chain keys (applies its own DB migrations on boot; needs DB: `.#db`, or `.#dev`)
        # `nix run .#cabinet-backend`→ cabinet BFF (:4000; needs piggybank on :50051; identity flows need concierge on :50061 from its own repo)
        # `nix run .#cabinet`        → Next.js host shell (:3000, proxies /api/* to the cabinet backend on :4000)
        # `nix run .#db`             → ensure the SHARED ev_invest Postgres is up (+ this repo's databases)
        # `nix run .#tb`        → local TigerBeetle only (banking-only, repo-local data)
        # `nix run .#new-replica` → move a PROD cluster replica to another host (recover, never format)
        # `nix run .#redis`     → ensure the SHARED ev_invest Redis is up
        # `nix run .#gen-api`   → regenerate contracts/openapi.json + cabinet TS types from the proto
        # `nix run .#concierge-pin-check` → assert the concierge contract pin is an ancestor of origin/main + bytes match (pin ↔ main ↔ cargo checkout, list: contracts/concierge-protos.txt; CI: .github/workflows/drift.yml)
        # `nix run .#drift-check` → assert Cargo.lock matches the manifests and the committed contract matches the protos (CI: .github/workflows/drift.yml)
        # `nix run .#check-rust`     → clippy (warnings denied) + workspace tests, the way CI runs them (CI: .github/workflows/checks.yml)
        # `nix run .#check-frontend` → cabinet tsc + tests against a clean `npm ci` (CI: .github/workflows/checks.yml)
        # Author new migrations with the sqlx CLI (in the dev shell):
        #   sqlx migrate add --source piggybank/core/migrations --sequential <name>
        apps = {
          init = { type = "app"; program = "${runInit}/bin/run-init"; };
          dev = { type = "app"; program = "${runDev}/bin/run-dev"; };
          piggybank = { type = "app"; program = "${runPiggybank}/bin/run-piggybank"; };
          signer = { type = "app"; program = "${runSigner}/bin/run-signer"; };
          cabinet-backend = { type = "app"; program = "${runCabinetBackend}/bin/run-cabinet-backend"; };
          cabinet = { type = "app"; program = "${runCabinet}/bin/run-cabinet"; };
          db = { type = "app"; program = "${runPostgres}/bin/run-postgres"; };
          tb = { type = "app"; program = "${runTigerbeetle}/bin/run-tigerbeetle"; };
          new-replica = { type = "app"; program = "${runNewReplica}/bin/new-replica"; };
          redis = { type = "app"; program = "${runRedis}/bin/run-redis"; };
          gen-api = { type = "app"; program = "${runGenApi}/bin/run-gen-api"; };
          concierge-pin-check = { type = "app"; program = "${runConciergePinCheck}/bin/run-concierge-pin-check"; };
          drift-check = { type = "app"; program = "${runDriftCheck}/bin/run-drift-check"; };
          check-rust = { type = "app"; program = "${runCheckRust}/bin/run-check-rust"; };
          check-frontend = { type = "app"; program = "${runCheckFrontend}/bin/run-check-frontend"; };
          publish = { type = "app"; program = "${runPublish}/bin/publish"; };
        };

        packages = {
          default = bankingBins;
          # The failover agent runs it on the host (devops fallback/agent.nix); only the one
          # binary, not the service closure around it.
          ledger-gap = pkgs.runCommand "ledger-gap" { meta.mainProgram = "ledger-gap"; } ''
            install -Dm755 ${bankingBins}/bin/ledger-gap $out/bin/ledger-gap
          '';
        } // containerStd.packages;

        containers = containerStd.containers;

        devShells.default =
          with pkgs;
          mkShell {
            shellHook =
              pre-commit-check.shellHook
              + combined.shellHook
              + ''
                cp -f ${(v_flakes.files.treefmt) { inherit pkgs; }} ./.treefmt.toml

                export DYLD_FALLBACK_LIBRARY_PATH="${rust}/lib''${DYLD_FALLBACK_LIBRARY_PATH:+:$DYLD_FALLBACK_LIBRARY_PATH}"
                export PROTOC="${pkgs.protobuf}/bin/protoc"

                # readme-fw regenerates README on every entry and has no wasm badge
                # key — append a WebAssembly badge after the docs.rs one, once.
                if [ -f ./README.md ] && ! grep -qi WebAssembly ./README.md; then
                  ${gnused}/bin/sed -i '/docs\.rs/a [<img alt="WebAssembly" src="https://img.shields.io/badge/WebAssembly-654FF0?logo=webassembly&logoColor=white" height="20">](https://webassembly.org)' ./README.md
                fi

                ${linkTbClient}
              '';

            packages = [
              nodejs
              redis
              openssl
              pkg-config
              protobuf
              clang-tools
              rust
              mold
              postgresql
              sqlx-cli
              tigerbeetleBin
              protocGenConnectOpenapi
              sccache
            ] ++ pre-commit-check.enabledPackages ++ combined.enabledPackages;

            env.RUST_BACKTRACE = 1;
            env.RUST_LIB_BACKTRACE = 0;
            # shared compile cache across builds; incremental off (sccache requires it).
            # Stays a devshell env var — in .cargo/config.toml it would break every
            # cargo invocation outside nix (CI runners have no sccache on PATH).
            env.RUSTC_WRAPPER = "sccache";
            env.CARGO_INCREMENTAL = "0";
          };
      }
    );
}
