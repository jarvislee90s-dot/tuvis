// 组件测试通用前置（M5 双语化引入的统一样板，原先逐测试文件重复一份）：
// 把浏览器语言钉到 zh-CN（navigator.languages / language）并把 i18n 切到 zh——
// 文案断言按 zh 值进行。只 changeLanguage 不够：jsdom 默认 en-US，组件拉 /host 做
// 连接时语言重评估（无语言字段 → 浏览器兜底）会把语言翻回 en，故浏览器语言必须一并钉住。
// 返回还原函数，afterAll 调用（恢复 jsdom 原生 navigator，避免跨文件污染）。
import i18n from "@/i18n";

export async function pinZhNavigator(): Promise<() => void> {
  Object.defineProperty(window.navigator, "languages", {
    value: Object.freeze(["zh-CN"]),
    configurable: true,
  });
  Object.defineProperty(window.navigator, "language", {
    value: "zh-CN",
    configurable: true,
  });
  await i18n.changeLanguage("zh");
  return () => {
    Reflect.deleteProperty(window.navigator, "languages");
    Reflect.deleteProperty(window.navigator, "language");
  };
}
