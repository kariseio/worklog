/* @refresh reload */
import { render } from "solid-js/web";
import App from "./App";
import Quick from "./Quick";
import "./styles.css";

// 같은 번들을 두 창이 쓴다: `?win=quick` 이면 빠른 메모 팝업, 아니면 메인.
const win = new URLSearchParams(window.location.search).get("win");
render(() => (win === "quick" ? <Quick /> : <App />), document.getElementById("root") as HTMLElement);
