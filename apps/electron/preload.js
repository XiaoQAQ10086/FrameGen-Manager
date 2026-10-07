/* FrameGen Manager —— Electron preload（渲染进程与主进程之间唯一的桥）

   这里暴露的对象**故意叫 window.__TAURI__**：现有前端 ui/app.js 只认这两个 API
    （core.invoke 和 event.listen），保持这个形状，那三个 UI 文件就能一个字节都不用改地
    搬过来 —— 「观感和动效一模一样」由此变成构造保证，而不是靠人去复刻。

   它只做转发，不含任何业务逻辑：业务全在 Rust core，命令在 sidecar 进程里执行。 */
const { contextBridge, ipcRenderer } = require('electron');

contextBridge.exposeInMainWorld('__TAURI__', {
  core: {
    // invoke(cmd, args) -> Promise<result>；失败时 reject(Error)，和 Tauri 行为一致
    invoke: function (cmd, args) {
      return ipcRenderer.invoke('fgm:invoke', cmd, args || {});
    },
  },
  event: {
    // listen(name, cb)：cb 收到的是 Tauri 那个形状的 { event, payload }
    listen: function (name, cb) {
      const channel = 'fgm:event:' + name;
      const handler = function (_e, payload) { cb({ event: name, payload: payload }); };
      ipcRenderer.on(channel, handler);
      return Promise.resolve(function () { ipcRenderer.removeListener(channel, handler); });
    },
  },
});
