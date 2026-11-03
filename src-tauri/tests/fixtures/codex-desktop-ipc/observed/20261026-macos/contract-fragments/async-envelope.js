var uxe = `<send_user_message_question_reply>`,
  dxe = `</send_user_message_question_reply>`,
  Ay = r.k({ questionItemId: r.P(), question: r.P(), answer: r.P() }),
  fxe = r.L([r.g(Ay).min(1), Ay.transform((e) => [e])]);
function pxe(e, t) {
  return JSON.stringify([`request_user_input_async`, e, t]);
}
function mxe(e) {
  let t = `\n${a.l}\n`,
    n = e.lastIndexOf(t),
    r = (n === -1 ? e : e.slice(n + t.length)).trim();
  if (!r.startsWith(uxe) || !r.endsWith(dxe)) return null;
  try {
    let e = fxe.safeParse(JSON.parse(r.slice(34, -35)));
    return e.success ? e.data : null;
  } catch {
    return null;
  }
}
