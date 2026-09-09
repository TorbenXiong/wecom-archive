import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import ServerApp from "./App";
import ClientApp from "./ClientApp";

const root = document.getElementById("root");
if (!root) throw new Error("missing application root");

const productTarget = import.meta.env.VITE_APP_TARGET === "client" ? "client" : "server";
document.body.dataset.product = productTarget;
const ProductApp = productTarget === "client" ? ClientApp : ServerApp;
createRoot(root).render(<StrictMode><ProductApp /></StrictMode>);
