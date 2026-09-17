/* @refresh reload */
import "./styles.css"; // 공용 스타일이 화면별 css 보다 먼저 번들에 들어가야 화면 css 가 이긴다.
import { render } from "solid-js/web";
import App from "./App";
import Quick from "./Quick";
import { applyCachedAppearance } from "./appearance";

// 설정을 읽어 오기 전에 지난번 모양(테마·글꼴·크기)을 먼저 입힌다 — 첫 그림이 깜빡이지 않게.
applyCachedAppearance();

// 같은 번들을 두 창이 쓴다: `?win=quick` 이면 빠른 메모 팝업, 아니면 메인.
const win = new URLSearchParams(window.location.search).get("win");
render(() => (win === "quick" ? <Quick /> : <App />), document.getElementById("root") as HTMLElement);
