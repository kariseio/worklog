/* @refresh reload */
import "./styles.css"; // 공용 스타일이 화면별 css 보다 먼저 번들에 들어가야 화면 css 가 이긴다.
import { render } from "solid-js/web";
import App from "./App";
import Quick from "./Quick";

// 같은 번들을 두 창이 쓴다: `?win=quick` 이면 빠른 메모 팝업, 아니면 메인.
const win = new URLSearchParams(window.location.search).get("win");
render(() => (win === "quick" ? <Quick /> : <App />), document.getElementById("root") as HTMLElement);
