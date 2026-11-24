// 被 sandbox 隔离的测试子页：主动验证能否借用父页设备权限，只命中内存计数器。
const config = JSON.parse(document.getElementById("config").textContent);
const base = new URL("./", import.meta.url);
const origin = base.origin;
const results = { module: true };
const diagnostics = [];
const notify = (phase, values) =>
  parent.postMessage({ type: "h09-report", run: config.run, phase, results: values }, "*");
const attackUrl = (kind) => origin + "/m/api/v1/probe/" + config.run + "/" + kind;
const show = () => {
  document.getElementById("child-output").textContent = JSON.stringify(
    { results, diagnostics },
    null,
    2
  );
};
if (new URL(location.href).searchParams.has("navigate")) {
  document.getElementById("start").hidden = true;
  document.getElementById("download").hidden = true;
  notify("navigation-started", { navigationStarted: true });
  location.assign(attackUrl("navigation-self"));
} else {
  document.getElementById("start").onclick = async (event) => {
    event.target.disabled = true;
    try {
      const response = await fetch(new URL("fetch.json", base), {
        credentials: "omit",
        signal: AbortSignal.timeout(5000),
      });
      results.fetch =
        response.status === 200 && (await response.json()).fixture === "h09-console-only";
    } catch (error) {
      diagnostics.push({ kind: "fetch", error: error.name });
    }
    try {
      results.ws = await new Promise((resolve) => {
        const url = new URL("ws", base);
        url.protocol = "ws:";
        const ws = new WebSocket(url);
        const timer = setTimeout(() => {
          ws.close();
          resolve(false);
        }, 5000);
        ws.onmessage = (event) => {
          if (event.data === "h09-console-ready") ws.send("h09-console-echo");
          if (event.data === "h09-console-echo") {
            clearTimeout(timer);
            resolve(true);
          }
        };
        ws.onerror = () => {
          clearTimeout(timer);
          resolve(false);
        };
        ws.onclose = (event) => {
          clearTimeout(timer);
          resolve(false);
          if (event.code === 1008 && Date.now() >= config.expiresAt)
            notify("expired-ws", { expiredWsClosed: true });
        };
      });
    } catch (error) {
      diagnostics.push({ kind: "ws", error: error.name });
    }
    try {
      const form = new FormData();
      form.append(
        "file",
        new File(["h09-memory-upload-only"], "h09-memory.txt", { type: "text/plain" })
      );
      const response = await fetch(new URL("upload", base), {
        method: "POST",
        credentials: "omit",
        body: form,
        signal: AbortSignal.timeout(5000),
      });
      const data = await response.json();
      results.upload = response.status === 200 && data.multipart && data.containsExpectedFile;
    } catch (error) {
      diagnostics.push({ kind: "upload", error: error.name });
    }
    try {
      const response = await fetch(new URL("download", base), {
        credentials: "omit",
        signal: AbortSignal.timeout(5000),
      });
      results.downloadBytes =
        response.status === 200 &&
        (await response.text()) === "H09 temporary download / 中文 / no real data\n";
    } catch (error) {
      diagnostics.push({ kind: "download-fetch", error: error.name });
    }
    // 不能把 fetch 的 CORS 错误视为拒绝；可信页另外读取实际 gate 的服务端记录。
    for (const [kind, init] of [
      [
        "simple-post",
        { method: "POST", headers: { "Content-Type": "text/plain" }, body: "counter-only" },
      ],
      ["no-cors-post", { mode: "no-cors", method: "POST", body: "counter-only" }],
      ["capability-query", { credentials: "omit" }],
    ]) {
      try {
        let url = attackUrl(kind);
        if (kind === "capability-query") url += "?cap=" + base.pathname.split("/")[2];
        const response = await fetch(url, {
          credentials: "include",
          ...init,
          signal: AbortSignal.timeout(5000),
        });
        diagnostics.push({ kind, status: response.status, type: response.type });
      } catch (error) {
        diagnostics.push({ kind, error: error.name });
      }
    }
    try {
      const frame = document.createElement("iframe");
      frame.hidden = true;
      frame.name = "h09-form-" + config.run;
      await new Promise((resolve, reject) => {
        const timer = setTimeout(() => reject(new Error("iframe timeout")), 5000);
        frame.onload = () => {
          clearTimeout(timer);
          resolve();
        };
        document.body.append(frame);
      });
      const form = document.createElement("form");
      form.method = "POST";
      form.action = attackUrl("form-post");
      form.target = frame.name;
      const input = document.createElement("input");
      input.name = "fixture";
      input.value = "counter-only";
      form.append(input);
      document.body.append(form);
      await new Promise((resolve) => {
        const timer = setTimeout(resolve, 5000);
        frame.onload = () => {
          clearTimeout(timer);
          resolve();
        };
        form.submit();
      });
      form.remove();
      frame.remove();
    } catch (error) {
      diagnostics.push({ kind: "form-post", error: error.name });
    }
    await new Promise((resolve) => {
      const ws = new WebSocket(
        origin.replace("http:", "ws:") + "/m/api/v1/ws/" + config.run + "/attack-ws"
      );
      const timer = setTimeout(() => {
        ws.close();
        resolve();
      }, 5000);
      ws.onopen = () => diagnostics.push({ kind: "attack-ws", opened: true });
      ws.onerror = () => diagnostics.push({ kind: "attack-ws", error: "WebSocketError" });
      ws.onclose = () => {
        clearTimeout(timer);
        resolve();
      };
    });
    try {
      void parent.document.body;
      results.parentDomDenied = false;
    } catch {
      results.parentDomDenied = true;
    }
    try {
      top.location.href = attackUrl("navigation-top");
      results.topNavigationDenied = false;
    } catch {
      results.topNavigationDenied = true;
    }
    try {
      results.popupDenied = window.open(attackUrl("navigation-popup"), "_blank") === null;
    } catch {
      results.popupDenied = true;
    }
    parent.postMessage({ type: "fetch", url: attackUrl("broker-fetch"), method: "POST" }, "*");
    parent.postMessage(
      {
        type: "h09-report",
        run: config.run,
        phase: "complete",
        results: {},
        url: attackUrl("broker-disguised"),
      },
      "*"
    );
    parent.postMessage(
      { type: "h09-report", run: "wrong-run", phase: "complete", results: {} },
      "*"
    );
    const nested = document.createElement("iframe");
    nested.hidden = true;
    nested.srcdoc =
      "<script>parent.parent.postMessage(" +
      JSON.stringify({
        type: "h09-report",
        run: config.run,
        phase: "complete",
        results: { module: true },
      }) +
      ", '*')<" +
      "/script>";
    document.body.append(nested);
    show();
    notify("complete", results);
  };
  show();
}
