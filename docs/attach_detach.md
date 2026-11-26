# アタッチ／デタッチの実装計画

状態: 計画（未実装）。2026-09-29 作成。決まっていない点は「9. 未決事項」にまとめています。

この文書では、今の `termway -- COMMAND`（端末と一緒に sway とアプリも終わる）を**単発モード**、新しく作る「端末から切り離しても残るもの」を**セッション**と呼びます。

## 1. 目的

- SSH が切れても、sway とアプリ（Firefox など）の状態を失わずに、あとから端末をつなぎ直せるようにする
- つなぎ直す端末は違ってよい（WezTerm で作ったセッションに Ghostty からつなぐ、など）
- distrobox 経由では、SSH が切れても SIGHUP も EOF も届かず、termway と sway とアプリが動き続ける。この問題もあわせて解決する

## 2. 実現性の確認（済み）

| 確認したこと | 結果 |
|---|---|
| 既存の `--attach` での再接続 | termway の外で sway＋foot を起動し、`termway --attach <socket>` を fake-term の下で 2 回続けて実行した。1 回目は WezTerm のモデルで 96×24 セル・20×42px（scale 2）、2 回目は kitty のモデルで 120×30 セル・9×21px（scale 1）。どちらも、取得したフレームと端末モデルの画面が 0 ピクセル差だった。1 回目に打ったコマンドの出力は、2 回目の画面にも残っていた。termway が終わっても sway は動き続け、出力のモードとスケールは、あとから接続した側の値に変わった |
| distrobox の中で切り離したプロセスが残るか | `distrobox enter termway -- script` の中で `setsid` したプロセスは、enter が戻ったあとも動き続ける（親は exec の conmon のまま） |
| ログアウトで runtime dir が消えないか | ホストは `Linger=yes`、`KillUserProcesses` は既定（no）。`/run/user/1000` は残る |
| コンテナの PID 名前空間 | `PidMode=host` |
| SSH の切断が届くか | distrobox 経由では届かない（conmon が pty を開いたままにする）。以前の調査で確認済み |

PoC の手順（`distrobox enter termway` の中で実行）:

```sh
RT=$XDG_RUNTIME_DIR/termway-poc; mkdir -m700 "$RT"
printf 'xwayland disable\noutput * mode --custom 1920x1080@60Hz\nexec foot\n' >"$RT/sway.conf"
setsid env WLR_BACKENDS=headless WLR_RENDERER=pixman WLR_LIBINPUT_NO_DEVICES=1 \
  XDG_RUNTIME_DIR="$RT" sway -c "$RT/sway.conf" </dev/null >"$RT/sway.log" 2>&1 &
fake-term --decode --emulate wezterm --cols 96 --rows 24 --cell 20x42 --duration 5 --snapshot a1.sim.ppm \
  --send-at '1.5:echo attach-one\r' -- termway --attach "$RT/wayland-1" --dump-frame a1.frame.ppm
fake-term --decode --cols 120 --rows 30 --cell 9x21 --duration 5 --snapshot a2.sim.ppm \
  --send-at '1.5:echo attach-two\r' -- termway --attach "$RT/wayland-1" --dump-frame a2.frame.ppm
fake-term --compare a1.frame.ppm a1.sim.ppm
fake-term --compare a2.frame.ppm a2.sim.ppm
```

分かったこと: Wayland がすでにクライアントとサーバーに分かれているので、描画の仕組みには手を入れずに済みます。タイルキャッシュや端末側の表示モデルは端末ごとのものです。再接続のときは、リサイズと同じように捨てて全画面を送り直せば足ります。

## 3. 方式の選択

| 案 | 判断 | 理由 |
|---|---|---|
| A. ホストプロセスがコンポジターを持ち、termway は Wayland のソケットにつなぎ直す | **採用** | 既存の `--attach` の経路がほぼそのまま使える。接続のたびにハンドシェイクをやり直すので、端末の種類・セルサイズ・互換モードが変わっても正しく描ける。切断中は誰もキャプチャしないので、termway の CPU 使用は 0 |
| B. tmux 型（サーバーがキャプチャと符号化を続け、クライアントはバイトを中継する） | 不採用 | 再接続すれば、結局ハンドシェイクと表示モデルの作り直しが要る。中継の層と独自のプロトコルの分だけ複雑になる。[DESIGN.md の 18 節](DESIGN.md#18-phase-5-のメモ範囲外)の分離構成とは目的が違う |
| C. termway を dtach・abduco・tmux の中で動かす | 不採用 | termway は再接続した端末のことを知らず、代替画面・マウス報告・互換モード・セルサイズが前の端末のままになる。tmux を通すと画像の配置が崩れる。切断中もキャプチャと符号化を続ける |

## 4. 設計

### 4.1 プロセスとファイル

```
 termway（クライアント。端末ごと・使い捨て）            termway __host（ホスト。setsid・端末なし）
   ハンドシェイク／キャプチャ／描画／入力 ──Wayland──>  sway (headless) ── アプリ
   client.lock を持つ                                   host.lock を持つ
                   $XDG_RUNTIME_DIR/termway/<name>/  （sway の XDG_RUNTIME_DIR）
```

セッションのディレクトリ（0700）には、今の `termway-<pid>-<n>` と同じファイルに加えて、いくつかのファイルを置きます。

| ファイル | 書く側 | 内容 |
|---|---|---|
| `wayland-1` | sway | ソケット |
| `bus` | ホスト | セッションバスへのリンク（今と同じ） |
| `launch.sh`・`sway.conf`・`app.status`・`compositor.log` | ホスト・起動スクリプト | 今と同じ |
| `session` | ホスト | `key=value` の行。コマンド、コンポジターの種類、作成時刻、ホストと sway の PID |
| `host.lock` | ホスト | 生きている間、排他の flock を持つ |
| `client.lock` | クライアント | アタッチしている間、排他の flock を持ち、自分の PID を書く |
| `compositor.status` | ホスト | sway の終了ステータス。`kill` で止めたときは `killed` |

- **名前**: `[A-Za-z0-9_-]` の 1〜32 文字に限ります（パスの組み立てに使うため）。ソケットのパスは 108 バイト未満に収まります。省略したときはアプリの名前（`firefox`、`foot`）を使い、重なれば `-2`、`-3` を付けます。
- **生死は flock で判定します。** `host.lock` の flock が取れれば、ホストは死んでいます（SIGKILL など）。PID の再利用に左右されません。flock はファイル記述ごとなので、同じプロセスの中でもテストできます。
- ロックのファイルは CLOEXEC で開き、子プロセスに渡しません。渡すと、子が生きている間ロックが外れなくなります。

### 4.2 セッションを作る（`termway new`）

1. クライアントが今と同じハンドシェイクをして、出力のモード（解像度・リフレッシュレート・スケール）を決める。`-d` のときは端末を使わず、`--size`（既定 1920x1080）とスケール 1 を使う
2. クライアントが `/proc/self/exe __host --name NAME --mode ... -- COMMAND` を起動する。スレッドがあるので fork は使わない。`pre_exec` で `setsid` し（制御端末を持たない）、stdin と stderr は `/dev/null`、stdout はパイプにする
3. ホストの処理:
   1. ディレクトリを作る。同じ名前があれば、ホストが生きていればエラー、残骸なら消す
   2. `host.lock` を取る
   3. `Compositor` を起動し、ソケットができるのを待つ（今と同じ 10 秒）
   4. `session` を書き、パイプに `ready <socket>` か `error <理由とログの末尾>` の 1 行を書いて閉じる
4. クライアントは、その 1 行を待つ（15 秒）。`ready` ならアタッチの手順に進み、`error` なら今の起動失敗と同じように表示して終わる
5. ホストは、SIGCHLD・SIGTERM・SIGINT を self-pipe で待つ。SIGHUP は無視する

- sway の `PR_SET_PDEATHSIG` は今と同じです。sway の親がホストになるので、ホストが SIGKILL されれば sway も止まります。
- アプリの環境変数は、`new` を実行したシェルのものになります。

### 4.3 アタッチ・デタッチ・横取り

アタッチ（`termway attach [NAME]`、または `new` の続き）:

1. 名前を解決する。省略したときは、セッションが 1 つならそれを、デタッチ中のものが 1 つだけならそれを使う。それ以外は一覧を出してエラーにする
2. `host.lock` で、ホストが生きているか確かめる。死んでいれば残骸を片付けてエラーにする
3. ハンドシェイクをする。横取りより先に行い、失敗したときに前のクライアントを追い出さずに済むようにする
4. `client.lock` を取る。取れなければ横取りする（下記）
5. ここからは今の `--attach` と同じで、ソケットにつなぎ、出力のモードを合わせ、キーマップと全画面を送る。ほかに次の 2 点を行う
   - ウィンドウの数も数える。`foreign_toplevel` は接続した時点で既存のウィンドウを通知するので、途中からつないでも数えられる
   - 統計に使うコンポジターの CPU 時間は、`session` に書かれた sway の PID から取る

デタッチ（`Exit::Detached`）:

- きっかけ:
  - デタッチキー
  - SIGHUP・SIGTERM・SIGINT
  - stdin の EOF・HUP
  - 生存確認の失敗（4.5 節）
  - 横取り（SIGUSR1）
- 端末は、今の終了と同じ手順で元に戻します。そのあと「`termway attach NAME` でつなぎ直せる」と表示します。
- 押している状態を記録しておき、切断の前に、押したままのマウスボタンを離します。キーは押下と解放を同時に送っているので、対応は要りません。
- セッションのクライアントでは、SIGTERM もデタッチとして扱います。セッションを終えるのは、`termway kill` とアプリの終了だけです。

横取り（別の端末からアタッチしたとき）:

1. `client.lock` に書かれた PID に SIGUSR1 を送る。ロックが取られている間は、その PID のプロセスがロックを持っている。PID がまだ書かれていなければ、少し待って読み直す
2. 前のクライアントは「別の端末でアタッチされた」と表示してデタッチする
3. 5 秒待ってもロックが外れなければ SIGKILL する。distrobox の切断で残ったクライアントは、端末への書き込みで止まっていることがあるため。クライアントを止めても、セッションには影響しない

### 4.4 セッションの終わり方

アタッチ中にアプリが終わったとき:

1. 起動スクリプトが `app.status` を書き、`swaymsg exit` する（今と同じ）
2. ホストの処理:
   1. sway が終わったのを見て、`compositor.status` を書く
   2. セッション内に残ったプロセスを止める（`stop_session_processes`）
   3. クライアントが `client.lock` を放すまで、最大 3 秒待つ
   4. ディレクトリを消して終わる
3. クライアントの処理:
   1. Wayland の切断を見て、`compositor.status` ができるのを最大 1 秒待つ
   2. `app.status`・ウィンドウの数・ログの末尾から、今と同じ説明（`exit_report`）を作って表示し、終わる

- デタッチ中にアプリが終わったときは、ホストがディレクトリを消して終わります。説明は残りません。
- `termway kill NAME` は、ホストに SIGTERM を送り、`host.lock` が外れるまで待ちます（5 秒）。ホストは、今の `shutdown` と同じ手順で sway とアプリを止めます。アタッチ中のクライアントは、「セッションが kill された」と表示して終わります。

### 4.5 切断の検知（生存確認）

distrobox 経由では、SSH が切れても SIGHUP も EOF も来ません。そこで、端末に DA1 を送って生きているかを確かめます。

- 端末から 10 秒間何も届かず、送るデータがなく、書き込みが止まっているときに `CSI c` を送る。応答に限らず、1 バイトでも届けば生きているとみなす。ハンドシェイクで DA1 に答えた端末だけが対象（答えない端末では、今もハンドシェイクでエラー終了する）
- 応答がないまま、送るデータがなかった時間の合計がタイムアウトを超えたら、切断とみなす。遅い回線で大きなフレームを送っている間は数えない
- 書き込みが進まない（書き込んだバイト数が増えない）時間がタイムアウトを超えた場合も、切断とみなす
- 切断とみなしたら、セッションならデタッチ、単発モードなら終了する（sway とアプリも止まる）
- オプションは `--liveness-timeout S`（0 で無効）。提案値はセッションで 30 秒、単発モードで 120 秒。単発モードでは、ノート PC のスリープなどで一時的に届かないだけでもアプリごと終わってしまうので、長めにする

これで、切断後に残る sway や Firefox も自動で片付きます。

## 5. CLI

```
termway [OPTIONS] [-- COMMAND]                      単発モード（今と同じ）
termway new [-s NAME] [-d] [--size WxH] [OPTIONS] [-- COMMAND]
termway attach [NAME] [OPTIONS]                     別名 a
termway ls                                          名前、コマンド、状態（attached PID／detached／stale）、作成からの時間
termway kill NAME
```

- セッションに固定されるもの（`new` でだけ指定できる）: COMMAND、`--compositor`、`--sway-config`、`--x11`、`--compositor-log`。`attach` に渡したらエラーにする
- アタッチのたびに変えられるもの:
  - 描画: `--phase`、`--tile`、`--cache-mb`、zlib のレベル、`--graphics-compat`、`--large-images`
  - 出力: `--scale`、`--max-fps`、`--output`、`--no-resize-output`
  - 入力と表示: `--mouse`、キー、`--status-line`、`--cell-size`
  - 記録: `--log`、`--stats`、`--dump-frame`
- 新しいオプション:
  - `--detach-key ctrl-\|none`。セッションでの既定は `ctrl-\`（dtach と abduco の既定と同じ）
  - `--liveness-timeout S`
- セッション中の `--quit-key` の既定は、9 節で決める
- 任意のコンポジターにつなぐ `--attach DISPLAY` は残す

## 6. 実装の手順

各ステップの終わりには、`cargo test` と e2e がすべて通る状態にします。

### Step 1: コンポジターの持ち方を分ける（振る舞いは変えない）

- `compositor.rs`
  - runtime dir を外から渡せる `spawn_at` を足す
  - `shutdown` を「プロセスを止める」と「ディレクトリを消す」に分ける
  - `exit_report` をクライアントからも使えるようにする（コンポジターの終了ステータスを整数で受け取る）
- `signals.rs`: 扱うシグナルを引数で渡せるようにする（クライアントは SIGUSR1 を、ホストは SIGCHLD を足す）
- `writer.rs`: 書き込んだバイト数（進み具合）を読めるようにする
- `app.rs`: コンポジターの持ち方を列挙型にする（自分で起動／セッション／外部）
- 完了条件: 既存のテストと e2e が、変更なしで通る

### Step 2: セッションの置き場とホスト

- `src/session.rs`（新規）: パス、名前の検証、`session` の読み書き、ロック、一覧、残骸の判定と片付け、横取り
- `src/host.rs`（新規）: `__host` の本体（4.2 節と 4.4 節）
- `main.rs`・`cli.rs`: `new -d`、`ls`、`kill`、`__host`（ヘルプには出さない）
- 完了条件:
  - `new -d` → `ls` → `kill` のあと、プロセスもディレクトリも残らない
  - ホストを SIGKILL したあと、`ls` がそのセッションを残骸として扱い、片付ける

### Step 3: アタッチとデタッチ

- `cli.rs`: `new`（`-d` なし）、`attach`、`--detach-key`
- `app.rs`
  - `Exit::Detached(理由)` と、デタッチのきっかけと表示
  - デタッチキー
  - 押したままのボタンを離す
  - 横取りされたとき（SIGUSR1）の処理
  - アプリが終わったときの説明（4.4 節）
- `session.rs`: 横取りの手順
- 完了条件: 7 節の e2e のうち、生存確認以外が通る

### Step 4: 生存確認

- `app.rs`: DA1 の送信とタイムアウト、書き込みが止まったときの判定（両方のモード）
- `fake-term`: 途中から応答をやめる `--mute-at S` と、途中から読まなくなる `--stop-reading-at S` を足す
- 完了条件: どちらの場合も、セッションならデタッチし、単発モードなら sway ごと終わる

### Step 5: 仕上げ

- 旧形式の残骸を扱う。対象は `$XDG_RUNTIME_DIR/termway-<pid>-<n>` のうち、その PID の termway がもういないもの（2026-09-29 時点で 3 つ残っている）
  - `ls` に `stale` として出し、`kill` で片付けられるようにする
  - 中にプロセスが残っていなければ、自動で消す
- 任意: デタッチしたまま一定時間たったらセッションを終える `--idle-timeout`（ホストが `client.lock` を定期的に確かめる）
- README（使い方、既知の制約）と DESIGN.md（判断の記録として新しい節）を更新する

規模の見込み: 新しいコードが 500〜700 行と、そのテストです。描画まわり（`render`、`sim`、`kitty`、`frame`）は変更しません。

## 7. テスト

ユニットテスト:

- 名前の検証（空、33 文字、`/`、`..`、`.` を拒否する）
- `session` の読み書き
- ロックと残骸の判定（同じプロセスの中で 2 回開いて flock する）
- ホストが返す行（`ready`／`error`）の解析
- 横取り（SIGUSR1 で終わる子プロセスと、SIGUSR1 を無視して SIGKILL で追い出される子プロセス）
- CLI（サブコマンド、セッションに固定されるオプションを `attach` に渡したときのエラー）
- 生存確認のタイムアウトの計算（書き込み中の時間を数えないこと）

e2e（`scripts/e2e.sh`）: 利用者のセッションと混ざらないように、`XDG_RUNTIME_DIR` を `target/e2e/rt` にします。アプリは foot だけを使います。

| ケース | 確認すること |
|---|---|
| `session_detach` | `new` → 何か打つ → デタッチキー。端末が元に戻り、`ls` に detached と出て、sway と foot が動いている |
| `session_reattach` | 別のサイズと WezTerm のモデル（20×42px）で `attach` する。0 ピクセル差で、前に打った内容が画面にある |
| `session_steal` | アタッチ中に、別の fake-term から `attach` する。前の fake-term は正常に戻って終わり、後の方は 0 ピクセル差 |
| `session_app_exit` | アタッチ中に `exit` する。クライアントが `foot exited with status 0` を出して終わり、ディレクトリもプロセスも残らない |
| `session_kill` | デタッチ中とアタッチ中のそれぞれで `kill` する。何も残らない |
| `session_stale` | ホストを SIGKILL する。`ls` が片付け、`attach` は分かりやすいエラーを出す |
| `liveness_session`・`liveness_oneshot` | fake-term が応答をやめる（`--mute-at`）と、前者はデタッチし、後者は sway ごと終わる |
| 既存のすべて | 変わらずに通る |

実機での確認:

- WezTerm（Windows）で `new -- firefox --new-instance --profile ...` を実行する
- SSH を切る。30 秒以内に `ls` の表示が detached になることを確かめる
- Ghostty（WSL2）から `attach` する

## 8. リスクと未確認事項

- デタッチの瞬間にマウスボタンが押されていると、sway 側で押されたままになるかもしれません。正常なデタッチでは、離してから切ります。クライアントが SIGKILL されたとき、仮想ポインタが消えたことで wlroots がボタンを離すかどうかは未確認です。
- アプリの環境変数は `new` を実行したときのものです。`SSH_AUTH_SOCK` などは、その SSH が切れると使えなくなります（tmux と同じ問題）。
- デタッチ中もアプリは動き続け、sway も描画を続けます（動画などは CPU を使います）。キャプチャはしないので、termway の CPU 使用は 0 です。
- 再アタッチのたびに出力のサイズとスケールが変わるので、アプリは描き直します（Firefox はレイアウトし直します）。
- 生存確認は誤判定することがあります。回線が一時的に止まっただけでも、切断とみなす場合があります。セッションならつなぎ直せば済みますが、単発モードではアプリごと終わってしまいます（そのため、タイムアウトを長めにします）。
- distrobox の切断で残ったクライアントは、終了時に端末を戻す書き込み（`restore`）で止まる可能性があります。横取りの SIGKILL と生存確認で抜けられるので、止まったままにはなりません。
- 横取りの SIGUSR1 は、`client.lock` を持っているプロセスにだけ送ります。PID のファイルを信じるのは、ロックが取られている間だけです。

## 9. 未決事項

| 項目 | 選択肢 | 推奨 |
|---|---|---|
| 既定の動き | (a) `termway -- CMD` は単発モードのままにして、残したいときだけ `new`／`attach` を使う (b) 常にセッションにする | (a)。今の使い方と e2e を変えずに済む。切断後に残ったものは生存確認で片付く |
| セッション中の終了キー | (a) 既定で無効（終えるのはアプリの終了か `kill`） (b) 今と同じ ESC | (a)。ESC の打ち間違いで Firefox ごと消えないようにする |
| 2 つ目の端末からのアタッチ | (a) 横取りする (b) 拒否する（横取りを指定するオプションを付けたときだけ横取りする） | (a)。distrobox の切断で残ったクライアントを、自動で追い出せる |
| 生存確認のタイムアウト | 4.5 節の提案値（セッション 30 秒、単発モード 120 秒） | 実機で調整する |

## 10. 範囲外

- 同じセッションを、複数の端末で同時に見ること（出力のサイズは 1 つしかないため）。見るだけのクライアントを `--no-resize-output` で足す余地はあります。
- デタッチ中に終わったアプリの説明を残すこと。
