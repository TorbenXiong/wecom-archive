import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import ServerApp from "./App";
import ClientApp from "./ClientApp";

const root = document.getElementById("root");
if (!root) throw new Error("missing application root");

const surface = import.meta.env.VITE_APP_TARGET === "client" ? "collector" : "workspace";
document.body.dataset.product = surface;
document.title = surface === "collector" ? "企业微信记录采集端" : "企业微信记录归档";
const ProductApp = surface === "collector" ? ClientApp : ServerApp;
createRoot(root).render(<StrictMode><ProductApp /></StrictMode>);
