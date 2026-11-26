# termway

SSH 先の Linux で動く Wayland GUI アプリを、手元のターミナル（Kitty Graphics Protocol 対応。想定は Ghostty）の中に描画する試作ツールです。

ヘッドレスの sway を起動し、wlr-screencopy で **変化した部分（damage）だけ** を取り出します。それをセル単位のタイルにして、**送信済みのタイルは再送せず配置命令だけ** を出し、**動いている間は半解像度** で送ります。**スクロールは、送信済みの画像をずらして表示し、新しく見えた帯だけ** を送ります。これにより、毎フレーム全画面を無圧縮で送る方式（Term.Everything）に比べて、転送量を桁違いに減らします。

```
 Ghostty ──ssh──> distrobox (Ubuntu) ─┬─ termway ── stdout: Kitty graphics ──> Ghostty
                                       │     ▲ stdin: キー / SGR マウス
                                       │     │ wlr-screencopy (damage)   仮想キーボード / ポインタ
                                       └─ sway (headless, pixman) ── foot などの Wayland アプリ
```

## 計測結果（要約）

282×60 セル、セル 9×21px（2538×1260px）、foot を使用。詳細・条件・生データは [docs/BENCHMARK.md](docs/BENCHMARK.md) を参照。

> **この表は初版（並列化とスクロールの最適化を入れる前）の計測です。** その後、計測環境の foot の既定フォントが大きくなり（行の高さ 13px → 17px）、同じ条件では再現しません。再計測はまだです。スクロールの最適化の暫定値は[下](#スクロールの最適化の暫定値)にあります。

| シナリオ | Term.Everything（等倍換算の計算値） | Phase 1 全画面 zlib | Phase 2 damage | Phase 3 タイルキャッシュ | Phase 4 ＋半解像度 |
|---|---:|---:|---:|---:|---:|
| (a) 静止 | 約 244 MiB/s | 470 KiB/s | **0 B/s** | **0 B/s** | **0 B/s** |
| (b) タイピング 10 文字/秒 | 約 244 MiB/s | 508 KiB/s | 4.2 KiB/s | **2.7 KiB/s** | 2.8 KiB/s |
| (c) `ls -R /usr` スクロール | 約 244 MiB/s | 5.63 MiB/s（18fps） | 6.43 MiB/s | 9.84 MiB/s ※ | **2.21 MiB/s**（17fps） |
| (c) foot のセルを 9×21 に揃えた場合 | 約 244 MiB/s | 5.33 MiB/s | 5.84 MiB/s | 3.82 MiB/s（28fps、ヒット率 99%） | **2.41 MiB/s** |
| (c) 回線 2.5MiB/s に制限 | 約 0.15fps（計算値） | 5.4 fps | 5.6 fps | 2.7 fps | **11.6 fps** |

※ foot の既定のセル（6×13px）は端末のセル（9×21px）と揃わないため、スクロールのたびに異なるタイルが大量に生まれ、Phase 3 は逆効果になります。foot を使うときはセルを揃えるのがおすすめです（[使い方](#使い方)）。

### スクロールの最適化の暫定値

less で 1 行ずつ、毎秒 30 行スクロールする「(d) ページスクロール」の Phase 4 です（現在の環境。詳細は [BENCHMARK.md](docs/BENCHMARK.md#暫定計測並列化とスクロールの最適化)）。

| 条件 | `--no-scroll-copy` | 既定（スクロールの最適化あり） |
|---|---:|---:|
| 200% 表示の WezTerm を最大化した大きさ（190×45 セル、20×42px、3800×1890px） | 1.73 MiB/s・15.0fps・半解像度 | **768 KiB/s・25.6fps・等倍** |
| 282×60 セル、9×21px（foot 既定） | — | **610 KiB/s・29.4fps**（Phase 3 は 4.39 MiB/s） |
| 282×60 セル、9×21px（foot のセルを揃える） | — | **408 KiB/s・29.8fps**（Phase 3 は 1.75 MiB/s） |

## 必要なもの

- サーバー: Linux と distrobox（Bluefin で確認）。GPU は不要（pixman でソフトウェア描画）
- コンテナ内: sway（1.9 と 1.11 で確認。`zwlr_screencopy_manager_v1` v2 以上と `zwlr_output_manager_v1` が必要）、表示したい Wayland アプリ（例: foot）、Rust 1.82 以降。`--x11` を使うときは Xwayland（24.1 で確認）と、X11 のウィンドウマネージャー（例: i3）
- クライアント: Kitty Graphics Protocol に対応したターミナル（Ghostty を想定。Sixel は不要）

## セットアップ（Bluefin + distrobox の Ubuntu）

ホスト（Bluefin）にはパッケージを追加しません。すべてコンテナ内に入れます。

```sh
# 1. Ubuntu コンテナを作る（既にあれば不要）
distrobox create --name termway --image docker.io/library/ubuntu:24.04
distrobox enter termway

# 2. コンテナ内: コンポジターとアプリ、ビルドに必要なもの
sudo apt update
sudo apt install --no-install-recommends sway foot wayland-utils build-essential curl ca-certificates

# 3. Rust（Ubuntu 24.04 の apt 版は古いので rustup を使う）
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
. "$HOME/.cargo/env"

# 4. ビルド（libwayland-dev は不要。pure Rust の wayland-client を使う）
cd /path/to/termway
cargo build --release
./target/release/termway --version
```

ホストの Rust でビルドしたバイナリは、ホストの glibc にリンクされてコンテナ内で動かないことがあります。**コンテナ内でビルドしてください。**

`wayland-info` は、プロトコルの対応状況を手で確かめるためのものです（任意）。

## 使い方

Ghostty から SSH で接続し、コンテナ内で実行します（`-t` で tty を割り当てる）。

```sh
ssh -t server 'distrobox enter termway -- /path/to/termway/target/release/termway'
```

コンテナ内のシェルから実行する場合:

```sh
termway                          # foot を起動（既定、Phase 4）
termway -- foot -e htop          # -- の後ろが sway 内で起動するコマンド
termway --status-line            # 最下行に転送量・fps・ヒット率を表示
termway --phase 3 --stats s.jsonl --log t.log
```

**foot を使うときのおすすめ設定**: foot のセルを端末のセルに揃えると、タイルキャッシュがほぼ全部当たります。9×21px の場合の例:

```sh
termway -- foot -o font=monospace:pixelsize=15 -o line-height=21px -o letter-spacing=1px
```

`pixelsize=15` だけではセル幅が 8px になるため、`letter-spacing=1px` で 9px にしています。ほかのセルサイズの場合は、`--dump-frame` で出力したフレームを見て調整してください。

- **終了**: ESC（既定）。ESC をアプリに送りたい場合は `--quit-key ctrl-]` を指定します。SIGINT/SIGTERM/SIGHUP や、アプリの終了でも終わります。どの場合も、端末の画像を削除し、代替画面から戻り、tty の設定を復元します。アプリが終了したときは終了コードをログに出し、失敗した場合やウィンドウを開かずに終わった場合は、アプリの出力の末尾も警告として出します。
- **Firefox などの単一インスタンスのアプリ**: 同じプロファイルの Firefox がすでに動いていると、新しいウィンドウはそちらに開き、起動したプロセスはすぐ終了します（termway も終わり、ログに `exited without opening a window` と出ます）。D-Bus のセッションバスを共有するので、別の termway セッションで動いている Firefox も対象です。別のプロファイルで起動してください:

  ```sh
  mkdir -p ~/.cache/termway-firefox
  termway -- firefox --new-instance --profile ~/.cache/termway-firefox
  ```
- **リサイズ**: ターミナルのサイズを変えると追従します（出力解像度を変え、キャッシュを捨てて全画面を再送）。
- **HiDPI**: セルが大きい端末（例: Windows の 200% 表示の WezTerm）では、出力のスケールを自動で上げます（`--scale`）。アプリの文字が端末の文字と同じくらいの大きさになります。
- **WezTerm**: WezTerm の Kitty graphics は、削除の一部を実装しておらず、同じ画像の配置の扱いも仕様と異なります。また、配置 ID つきの画像があるセルに別の画像を置くと、その画像がセルの中で複製され、繰り返すと指数的に増えて WezTerm が応答しなくなり落ちます（[DESIGN.md](docs/DESIGN.md#wezterm-互換モード--graphics-compat)）。WezTerm 用の出力は、そのような置き方をしません。XTVERSION で WezTerm と分かれば、自動で WezTerm 用の出力に切り替えます。Windows の `ssh.exe` 経由などで判別できないときは、`--graphics-compat wezterm` を指定してください（ログに `graphics compat` の行が出ます）。
- **Ghostty**: Ghostty 1.3 は、大きな zlib 圧縮の画像を展開するときに落ちることがあります（Ghostty が使う Zig 0.15 の標準ライブラリの不具合。main では修正済み、未リリース）。XTVERSION で Ghostty と分かれば、32 KiB を超える画像（半解像度の画像やスクロールの帯）は PNG で送ります。Ghostty は PNG を別の展開器（wuffs）で読むので、この不具合に当たりません。ログに `images over 32 KiB sent as PNG` の行が出ます。判別できないときは `--large-images png` を指定してください（[DESIGN.md](docs/DESIGN.md#ghostty-13-大きな画像は-png-で送る--large-images)）。
- **既存のコンポジターを使う**: `termway --attach wayland-1`（`$XDG_RUNTIME_DIR` からの相対名、またはソケットのパス）。この場合、アプリは起動しません。

### sway のデスクトップを使う（`--sway-config`）

`--sway-config` で自分の sway の設定を読み込ませると、sway のデスクトップ（複数のウィンドウ、タイル分割、バー）を端末の中で使えます。termway が起動する sway に設定を足すだけなので、sway を入れ子にしません。

```sh
termway --quit-key ctrl-] --sway-config ~/.config/sway/termway.conf
```

設定の例（`~/.config/sway/termway.conf`）:

```
# Windows キー（Super）は Windows に取られて端末に届かないので、Alt を使う
set $mod Mod1
output * bg #285577 solid_color
bindsym $mod+Return exec foot
bindsym $mod+Shift+q kill
bindsym $mod+Shift+e exit
bar {
    position top
    status_command while date +'%H:%M:%S'; do sleep 1; done
}
exec foot
```

- **`--` の後ろにコマンドを書かなければ、termway はアプリを起動しません**（設定の `exec` で起動します）。セッションは sway の終了（上の例では Alt+Shift+e）か、終了キーで終わります。foot を閉じても終わりません。コマンドを書いた場合は、これまでどおり、そのアプリが終わるとセッションも終わります。
- **sway の既定の設定（`/etc/sway/config`）も `~/.config/sway/config` も読み込みません。** 使うのは termway の既定の設定と、指定したファイルだけです。見た目（隙間や色）だけを書いたファイルを渡すと、`exec` もキー割り当てもないので、画面は真っ黒のままです。5 秒たってもウィンドウが 1 つも開かなければ、ログに `no window has opened` と出ます。見た目の設定を別のファイルに分けたいときは、上の例のようなデスクトップ用の設定の最後で `include ~/.config/sway/config` のように読み込んでください。
- 設定は termway の既定（枠なし、Xwayland 無効、ポインタを隠すなど）のあとに読み込むので、設定ファイルの値が優先されます。ただし、出力の解像度とスケールは、接続したあとに termway が端末に合わせて設定し直します（`--scale`）。設定ファイルの `output` の `mode` と `scale` は効きません。
- 設定の誤りは、ログに `sway config: Error on line …` と出ます（sway も画面の上に赤いバーを出します）。**sway の設定では、コメントは行を分けて書いてください。** 行末に `# …` を書くと、その行はまるごとエラーになります。
- 設定ファイルの中の相対パスの `include` は、その設定ファイルの場所から探します。
- `--compositor sway` のときだけ使えます。`--attach` とは併用できません。
- 既定の終了キー ESC は、Alt+キーとは区別されます（ESC の直後に文字が続けば Alt として扱う）。ただし vim などで ESC を使うなら `--quit-key ctrl-]` にしてください。
- Ubuntu の既定の `/etc/sway/config` は、壁紙（sway-backgrounds パッケージ）がないとエラーになり、Super キーを使います。上の例のような専用の設定を使ってください。

`-- sway -c FILE` で sway の中に sway を起動する（入れ子にする）こともできますが、おすすめしません。sway 1.11（wlroots 0.19）では、内側の sway の画面が、描き直すたびに表示と真っ黒を交互に繰り返します（[既知の制約](#既知の制約)）。また、内側の sway が合成した画面を外側の sway がもう一度合成するので、CPU も余計に使います。

### X11 のウィンドウマネージャーを使う（`--x11`、i3 など）

`--x11` を付けると、コマンド（既定は `i3`）を X11 のセッションとして動かします。termway が起動する sway の中で、X の画面全体を 1 つのウィンドウとして表示する Xwayland（rootful）を起動し、その上でコマンドを実行します。

```sh
sudo apt install xwayland i3 xterm      # コンテナ内で。i3 の中の端末には X11 のもの（xterm など）が必要
termway --quit-key ctrl-] --x11                        # ~/.config/i3/config（なければ i3 の既定）で i3 を起動
termway --quit-key ctrl-] --x11 -- i3 -c ~/i3-termway.conf
termway --quit-key ctrl-] --x11 -- openbox-session     # ほかの X11 のウィンドウマネージャーやデスクトップ
```

- **X の画面は、端末の物理ピクセルと同じ大きさです。** HiDPI の端末（スケール 2 や 1.5）でもくっきり描かれ、端末の大きさを変えると X の画面も追従します。
- X のアプリは画面のスケールを知らないので、文字の大きさは DPI で合わせます。termway は Xwayland に `96 × スケール` の DPI（スケール 2 なら 192）を伝えます。Xft や pango でフォントを描くアプリ（i3 のバー、`xterm -fa monospace -fs 10`、GTK、Qt、Firefox）は、その DPI に合わせて文字を大きくします。X の昔ながらのビットマップフォント（xterm の既定など）は小さいままです。DPI は起動時の値のままで、途中でスケールが変わっても変わりません。
- X のセッションには、termway が作った認証用の cookie（`XAUTHORITY`）を持つアプリだけが接続できます。同じサーバーのほかのユーザーが、画面を読んだりキー入力を送り込んだりすることはできません。
- セッションは、コマンド（i3）の終了で終わります。i3 の既定の設定では Alt+Shift+E を押し、上に出るバーの「Yes, exit i3」をクリックします。Xwayland も一緒に止まります。自分の i3 の設定がないと、初回は i3-config-wizard が設定を作るか尋ねます。
- コマンドの環境からは `WAYLAND_DISPLAY`、`SWAYSOCK`、`I3SOCK` を外し、`XDG_SESSION_TYPE=x11` にします。そうしないと、Wayland 対応のアプリ（foot など）が X の画面ではなく外側の sway に開き、i3 は sway の IPC ソケットを自分のものと取り違えて起動できません。
- **i3 のキー（$mod）には Alt（Mod1）を使ってください。** Windows キー（Super）は Windows や macOS に取られて端末に届きません。さらに端末によって、Alt と組み合わせたキーが届かないことがあります。
  - WezTerm は、既定で Alt+Enter を全画面の切り替えに使うので、i3 の既定の `$mod+Return`（端末を開く）が届きません。WezTerm の設定で `{ key = 'Enter', mods = 'ALT', action = wezterm.action.DisableDefaultAssignment }` を `keys` に足すか、i3 で別のキーに割り当ててください。WezTerm は Alt+Shift+Enter を Alt+Enter と同じバイトで送るので、termway からは Alt+Enter に見えます。
  - macOS の Ghostty は、既定では Option を Alt として送りません（Option+B は `∫` になる）。Ghostty の設定に `macos-option-as-alt = true` を足してください。Ghostty は Shift+Enter や Alt+Shift+Enter を区別して送るので、`$mod+Shift+Return` のような割り当ても使えます。
- sway は i3 の設定をほぼそのまま読めるので、i3 の設定を `--sway-config` に渡すだけでも、似たデスクトップになります（X のアプリは動きません）。

### 主なオプション

| オプション | 既定 | 説明 |
|---|---|---|
| `--phase N` | 4 | 1 全画面 / 2 damage 矩形 / 3 タイルキャッシュ / 4 タイル＋動いている間は半解像度＋スクロールは送信済みの画像をずらす |
| `--compositor sway\|cage` | sway | cage 0.1.5 は出力モード変更で落ちる（既知の制約を参照） |
| `--x11` | | コマンド（既定は `i3`）を X11 のセッションとして、rootful の Xwayland の上で動かす |
| `--sway-config FILE` | | sway の設定に FILE を読み込む（バー、キー割り当て、隙間、`exec` など）。コマンドは省略でき、そのときはアプリを起動せず、sway の終了でセッションが終わる |
| `--scale auto\|F` | auto | 出力のスケール（1〜4）。auto はセルの高さ ÷ 21px を 0.25 刻みに丸める（HiDPI の端末でアプリの文字が小さくならないように） |
| `--graphics-compat auto\|kitty\|wezterm` | auto | 画像の配置と削除の方式。auto は XTVERSION の応答が WezTerm なら wezterm、それ以外は kitty（仕様どおり） |
| `--large-images auto\|zlib\|png` | auto | 32 KiB を超える画像の送り方。auto は XTVERSION の応答が Ghostty なら png、それ以外は zlib（RGB を zlib で圧縮） |
| `--max-fps N` | 30 | キャプチャの上限。コンポジターは max(60, N) Hz で動かす |
| `--tile WxH` | 1x1 | タイルの大きさ（セル単位） |
| `--cache-mb N` | 128 | 端末側に置くタイル画像の予算（4B/px で見積もり） |
| `--zlib-level N` / `--tile-zlib-level N` | 6 / 1 | 大きな画像用 / タイル用の圧縮レベル（0 で無圧縮） |
| `--no-solid-fill` | | 単色タイルもセル背景ではなく画像で送る |
| `--lowres-threshold F` / `--refine-delay-ms N` | 0.05 / 200 | Phase 4 で「動いている」とみなす変化率 / 高画質化までの静止時間 |
| `--no-scroll-copy` | | Phase 4 のスクロールの最適化（送信済みの画像をずらして表示する）を使わない |
| `--threads N` | CPU 数（最大 8） | フレームの変換・ハッシュ・圧縮に使うスレッド数 |
| `--status-line` | | 最下行を統計表示に使う（出力は 1 行分小さくなる） |
| `--quit-key esc\|ctrl-]\|none` | esc | 終了キー |
| `--mouse auto\|pixels\|cells\|off` | auto | SGR-Pixels（1016）に対応していればピクセル座標を使う |
| `--cell-size WxH` | | セルサイズの検出を飛ばす |
| `--skip-graphics-check` | | Kitty graphics の対応確認（`a=q`）を飛ばす |
| `--log FILE` / `--stats FILE` | | ログ / 秒ごとの統計（JSON Lines） |
| `--compositor-log FILE` | | コンポジターの標準出力と標準エラー |
| `--dump-frame FILE` | | 終了時に、最後に取得したフレームを PPM で保存 |

すべてのオプションは `termway --help` で確認できます。

## 既知の制約

- **実機の Ghostty での確認は始めたばかりです。** 開発したサーバーに Ghostty がないためです。macOS の Ghostty から「遅い」「画像がところどころ欠け、そのうち落ちる」という報告がありました。欠けと落ちるのは Ghostty 1.3 の zlib 展開の不具合と特定し、大きな画像を PNG で送るようにしましたが、実機ではまだ確かめていません。WezTerm 用の出力は、WezTerm のソースから作ったモデルで確認したものです。代わりに、Kitty graphics の仕様に沿った端末モデル（`src/sim.rs`）と、pty 上の疑似端末（`fake-term`）で、表示がピクセル単位で一致することを確認しています。
  - 実機で確認が必要なもの: `d=c`/`d=q`/`d=y`/`d=z`/`d=R` の削除、同じ z の画像 ID 順、`c=`/`r=` による拡大、DEC 2026、SGR-Pixels の座標原点
  - 詳細は [docs/DESIGN.md](docs/DESIGN.md#16-未確認事項前提にしていないことまたは実機で確認が必要なこと)
- `ext-image-copy-capture-v1` は sway 1.9（wlroots 0.17）にないため、wlr-screencopy だけを使います。
- **sway 1.11（wlroots 0.19）で sway を入れ子にすると、内側の画面が描き直すたびに真っ黒になります。** `-- sway -c FILE` で起動した内側の sway の画面が、描き直しのたびに「全部映る」と「完全に真っ黒」を交互に繰り返します（バーの時計があると 1 秒おきにちらつく）。termway が取得した時点で真っ黒で、termway の処理によらず起きます。原因は特定できていません（[DESIGN.md](docs/DESIGN.md#19-技術的負債今後)）。`--sway-config` を使ってください。
- cage 0.1.5 は、出力モードを変えると assert で落ちます。`--compositor cage --no-resize-output` なら動きますが、1920×1080 固定になり、端末に合わせて切り取るか黒で埋めることになります。
- 既定の ESC 終了を使うと、ESC をアプリに送れません（vim などは `--quit-key ctrl-]` を使う）。
- 入力は従来型の端末エンコードを解析しています。
  - Ctrl+Shift+英字や Shift+Enter のように、従来型のエンコードでは区別されない組み合わせは、端末が区別して送る場合（xterm の modifyOtherKeys 形式の `CSI 27;…~` と `CSI …u`。Ghostty は Shift/Ctrl/Alt+Shift+Enter をこの形で送る）だけ届きます。termway から端末に拡張キーボードプロトコルを要求はしていません
  - キーは押下と同時に解放するため、キーリピートや長押しは再現できません
  - キーボード配列は US 固定です。US 配列にない文字（IME の確定文字列など）は、キーマップを動的に追加して送ります（Xwayland 無効のため X11 アプリは対象外）
- headless 出力にはハードウェアカーソルがないため、sway のポインタがフレームに描き込まれます。1 秒操作がないときとタイピング中は隠します。アプリが指定するカーソル形状は、端末側のポインタには反映されません。
- 画面全体が大きく変わり続ける場合（`ls -R` を流し続けるなど）は CPU で律速します。Xeon E5-2640 v3 の 3800×1890px で約 16fps です。変換・ハッシュ・圧縮は並列化しましたが、sway の pixman 描画と screencopy のコピーにも 1 フレーム 40ms 前後かかります。
- スクロールの最適化は、縦方向のスクロールだけが対象です。範囲は画面の全幅のタイル行で、1 フレームのずれが範囲の高さの 1/4 を超える場合は、従来どおり半解像度で描きます。
- **Ghostty では、画面上の画像の数に比例して描画が重くなります。** Ghostty（1.3.1 と 2026-09 時点の main）は、配置 1 つごとに GPU のバッファを作って描画命令を出します。termway は文字のあるセルごとにタイルを 1 つ置くので、文字の多い画面では配置が数千になります（190×50 セルの文章で約 1,600）。WezTerm は画像をまとめて描くので、この影響を受けません。対策は検討中です（[DESIGN.md](docs/DESIGN.md#19-技術的負債今後)）。
- 端末側の画像メモリは推定値で管理しています（`--cache-mb`）。端末の上限（kitty と Ghostty の既定は 320MB）より十分小さくしてください。
- SIGKILL で終了した場合は、端末を復元できません（コンポジターは PDEATHSIG で終了します）。
- **distrobox（rootless の podman）の中では、sway のデスクトップで X のアプリを 1 つずつ動かす（rootless の Xwayland、設定の `xwayland enable`）ことはできません。** コンテナの中から見ると、ホストの `/tmp/.X11-unix` の持ち主が `nobody` になり、wlroots がその場所での Xwayland の起動を断るためです（`/tmp/.X11-unix not owned by root or us`）。X のアプリは `--x11` で使ってください。
- クリップボードの連携はありません。

## 開発

```sh
cargo test                 # ユニットテスト 159 件（pty を使うテストも含む。コンポジターは不要）
scripts/e2e.sh             # 実際の sway と foot で、全 Phase・リサイズ・HiDPI・WezTerm モデル・ページスクロール・ポインタを重ねたままのスクロール・入れ子の sway・`--sway-config` のデスクトップ・`--x11` の i3・終了経路を検証（コンテナに i3・xterm・Xwayland が必要）
scripts/bench.sh           # 計測（4 Phase × シナリオ）→ target/bench/summary.md
```

### 計測

```sh
OUT=target/bench/default scripts/bench.sh
OUT=target/bench/aligned APP="foot -o font=monospace:pixelsize=15 -o line-height=21px -o letter-spacing=1px" scripts/bench.sh
OUT=target/bench/throttled THROTTLE_KBPS=2560 SCENARIOS="typing scroll pagescroll" scripts/bench.sh
# 200% 表示の WezTerm を最大化した大きさ。スクロールの最適化の有無を比べる
OUT=target/bench/hidpi COLS=190 ROWS=45 CELL=20x42 PHASES=4 SCENARIOS="scroll pagescroll" scripts/bench.sh
OUT=target/bench/hidpi_off COLS=190 ROWS=45 CELL=20x42 PHASES=4 SCENARIOS="scroll pagescroll" scripts/bench.sh --no-scroll-copy
```

- シナリオ: `static`（静止）、`typing`（10 文字/秒）、`scroll`（`ls -R /usr` を流し続ける）、`pagescroll`（less で 1 行ずつ、毎秒 30 行）。環境変数 `COLS`/`ROWS`/`CELL` で端末の大きさを変えられます。`bench.sh` の引数は termway にそのまま渡します。

- `fake-term` は pty 上で Ghostty の代わりをする計測用のハーネスです。`fake-term --help` で使い方を表示します。
  - 起動時の問い合わせに応答する（応答しない・不正な応答をするモードもある）
  - 入力シナリオを流し、termway の出力バイトを数える
  - 回線速度を制限する
  - `--decode` で端末モデルを動かし、プロトコル違反を検出する。画面のスナップショットも保存できる
  - 既定では XTVERSION に Ghostty 1.3.1 として応答し、`--decode` では、32 KiB を超える zlib 画像を Ghostty 1.3 が落ちる入力としてエラーにする
  - `--emulate wezterm` で、WezTerm の画像の扱いを再現し、XTVERSION に WezTerm として応答する
- `scripts/analyze_stream.py` は、`fake-term --dump-output` で保存した出力を、コマンドの種類ごとに分類します。
- `examples/zbench.rs` は、実フレームを使った zlib の圧縮レベルとバックエンドの比較です。

### ソースの構成

| ファイル | 役割 |
|---|---|
| `src/app.rs` | メインループ（poll、キャプチャ、入力、リサイズ、後始末） |
| `src/render.rs` | Phase 1〜4 の描画戦略と、端末側の表示モデル |
| `src/render/region.rs` / `src/scroll.rs` | スクロールの最適化（範囲の管理 / 行ハッシュによるずれの検出） |
| `src/par.rs` / `src/zlib.rs` | スレッドでの分割 / zlib（大きな入力は分割して並列に圧縮） |
| `src/kitty.rs` / `src/vt.rs` / `src/base64.rs` | Kitty graphics と VT の出力 |
| `src/geom.rs` | 矩形、damage のセル境界への拡張と結合、タイル分割 |
| `src/lru.rs` | ピン留めつき LRU |
| `src/frame.rs` | shm から RGB への変換、切り出し、半解像度化、単色判定 |
| `src/wl.rs` | screencopy、仮想キーボード/ポインタ、output-management |
| `src/compositor.rs` | sway/cage の起動と専用 runtime dir |
| `src/term.rs` / `src/input.rs` / `src/keymap.rs` | raw モードとハンドシェイク / 入力の解析 / XKB キーマップ |
| `src/writer.rs` / `src/signals.rs` / `src/stats.rs` / `src/log.rs` / `src/cli.rs` | 出力スレッド、シグナル、統計、ログ、引数 |
| `src/sim.rs` | テストと計測に使う端末モデル |
| `src/bin/fake-term.rs` | 計測と E2E 用の疑似端末 |

設計判断は [docs/DESIGN.md](docs/DESIGN.md) にまとめています（Phase 5 の分離構成のメモもここにあります）。

## ライセンス

MIT License と Apache License 2.0 のどちらかを選んで使えます。条文は [LICENSE-MIT](LICENSE-MIT) と [LICENSE-APACHE](LICENSE-APACHE) にあります。
