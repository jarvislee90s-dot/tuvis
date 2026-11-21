//! 无副作用的关闭计划器：只做保守校验与目标选择，不执行任何 Invoke / 写库 / 状态变更。
//!
//! 守卫顺序（任一失败立即返回对应 `Refuse`，无法证实一律保守拒绝）：
//! 1. 绑定完整性：服务端签发的 lease 非空，且绑定组身份与请求组身份完整相等；
//! 2. 租约时效：`now_ms >= expires_at_ms` 即拒绝（精确边界视为已过期）；
//! 3. 作用域一致：进程代际(pid+started_at)/窗口(hwnd)/包指纹/视图路径/epoch 全部字段相等；
//! 4. 来源完整且成员恰一题（来源单题不能替代原生完整题数）；
//! 5. 快照完整：枚举有截断/失败/超时的快照既不能定位卡，也不能证实缺席；
//! 6. 绑定形态分流：
//!    - `ExplicitPairing` 必须绑定非空 `card_runtime_id`，且该物理 RuntimeId 在快照中
//!      恰有一张活卡（禁止按文本唯一性猜线程；重复 → Ambiguous；缺失 → 绑定元素
//!      消失/陈旧替换/错代，不得当作 NotPresentVerified）；
//!    - `IndependentContext` 不得携带绑定卡字段，仅在完整同 scope 卡列表为空时
//!      可 `NotPresentVerified`；
//! 7. 命中卡守卫（顺序：内容一致 → 原生题数 → dismiss 形态 → v2 指纹）：
//!    - 完整 title + 有序 options 与来源成员一致；
//!    - 原生完整题数恰为 `Some(1)`（None → NativeCountUnknown，0/多题 → UnsupportedGroup：
//!      多题 Skip 可能提交其他草稿，题数证据后续由后端/显式配对产生，不在此默造）。
//!      该检查先于指纹：题数未知/多题必须给出具体理由，不能被"新指纹不匹配"覆盖；
//!    - dismiss 形态：直属 Dismiss 且支持 Invoke、RuntimeId 非空且异于卡 RuntimeId。
//!      该形态检查也先于指纹：语义不成立（空/同卡）→ InvokeUnavailable；成立但与
//!      绑定不一致（如 dismiss id 变化）→ 落到指纹 ContentChanged；
//!    - 绑定指纹等于签发与 planner 共用的 v2 规范
//!      `helper::card_fingerprint_v2`（绑定题文、题数证据、lease_id、完整 scope 与
//!      card/dismiss RuntimeId）；不一致或序列化失败一律保守 ContentChanged。
//!
//! 成功时输出"实测"的 scope/card/dismiss 目标，供执行器在调用前重读核对。

use super::helper::card_fingerprint_v2;
use super::types::{
    BindingKind, ClosePlan, GroupSelector, InvokeTarget, NativeCard, NativeCardSnapshot,
    QuestionFingerprint, SkipReason, ThreadBindingProof,
};

pub(crate) fn plan_close(
    selector: &GroupSelector,
    proof: &ThreadBindingProof,
    snapshot: &NativeCardSnapshot,
    now_ms: u64,
) -> ClosePlan {
    // 1. 绑定完整性：lease 必须由服务端签发且非空；组身份四个字段必须完整相等
    //    （thread/turn/item/generation 任一不符即视为该绑定不覆盖本组）。
    if proof.lease_id.is_empty() || proof.group != selector.group {
        return ClosePlan::Refuse(SkipReason::BindingRequired);
    }
    // 2. 租约时效：精确边界 now == expires_at_ms 已过期。
    if now_ms >= proof.expires_at_ms {
        return ClosePlan::Refuse(SkipReason::LeaseExpired);
    }
    // 3. 作用域一致：快照必须仍处于绑定时的完整窗口作用域，任何字段漂移都拒绝。
    if proof.scope != snapshot.scope {
        return ClosePlan::Refuse(SkipReason::ScopeChanged);
    }
    // 4. 来源完整性：来源未收敛不动作；来源成员必须恰一题（多题组不支持原生关闭）。
    if !selector.source_complete {
        return ClosePlan::Refuse(SkipReason::SourceIncomplete);
    }
    if selector.members.len() != 1 {
        return ClosePlan::Refuse(SkipReason::UnsupportedGroup);
    }
    let member = &selector.members[0];
    // 5. 快照完整性：不完整快照不能支撑任何结论（含缺席验证）。
    if !snapshot.complete {
        return ClosePlan::Refuse(SkipReason::NativeUnavailable);
    }
    // 6. 绑定形态分流。
    match proof.kind {
        BindingKind::ExplicitPairing => {
            // 显式配对必须绑定具体卡；空绑定不能借同文文本冒充线程证据。
            if proof.card_runtime_id.is_empty() {
                return ClosePlan::Refuse(SkipReason::BindingRequired);
            }
            let matches: Vec<&NativeCard> = snapshot
                .cards
                .iter()
                .filter(|card| card.runtime_id == proof.card_runtime_id)
                .collect();
            match matches.len() {
                // 按服务端绑定的完整物理 RuntimeId 精确选择，不按文本唯一性猜。
                1 => invoke_guard(member, proof, matches[0]),
                // 绑定元素已从完整快照中消失：陈旧替换/消失/错代，租约失效，
                // 不得借空快照或同文历史卡报 NotPresentVerified。
                0 => ClosePlan::Refuse(SkipReason::NativeUnavailable),
                // 同一物理 RuntimeId 出现多张活卡：无法唯一定位。
                _ => ClosePlan::Refuse(SkipReason::Ambiguous),
            }
        }
        BindingKind::IndependentContext => {
            // 独立上下文绑定不得携带卡字段：卡级定位/关闭需要显式配对证明。
            if !proof.card_runtime_id.is_empty() {
                return ClosePlan::Refuse(SkipReason::BindingRequired);
            }
            if snapshot.cards.is_empty() {
                // 完整同 scope 快照确无任何卡：当前已核对范围内未呈现。
                // （含义是"已核对未呈现"，不是原生永久关闭。）
                ClosePlan::NotPresentVerified
            } else {
                // 现场仍有卡片：同文卡无法归属线程（多义不自动选），其他组在场也无法
                // 证实目标组缺席 → 状态不可判定，保守拒绝。
                ClosePlan::Refuse(SkipReason::Ambiguous)
            }
        }
    }
}

/// 命中卡的逐项守卫：内容一致 → 原生题数 → dismiss 形态 → v2 指纹。
/// （题数与 dismiss 形态先于指纹：NativeCountUnknown / UnsupportedGroup /
///   InvokeUnavailable 必须给出具体理由，不能被"新指纹不匹配"覆盖。）
fn invoke_guard(
    member: &QuestionFingerprint,
    proof: &ThreadBindingProof,
    card: &NativeCard,
) -> ClosePlan {
    // 1. 完整 title 与有序 options 必须与来源成员一致（顺序敏感，不允许乱序等价）。
    if card.question.title != member.title || card.question.options != member.options {
        return ClosePlan::Refuse(SkipReason::ContentChanged);
    }
    // 2. 原生完整题数证据：仅 Some(1) 放行。None = 题数未知（来源单题不能替代原生
    //    题数）；Some(0)/Some(>1) = 非单题面板，多题 Skip 可能提交其他草稿。
    match card.native_question_count {
        Some(1) => {}
        None => return ClosePlan::Refuse(SkipReason::NativeCountUnknown),
        Some(_) => return ClosePlan::Refuse(SkipReason::UnsupportedGroup),
    }
    // 3. 关闭按钮必须是该卡直接子节点、支持 Invoke，且非空、异于卡 RuntimeId。
    if !card.dismiss_is_direct_child || !card.invoke_supported {
        return ClosePlan::Refuse(SkipReason::InvokeUnavailable);
    }
    if card.dismiss_runtime_id.is_empty() || card.dismiss_runtime_id == card.runtime_id {
        return ClosePlan::Refuse(SkipReason::InvokeUnavailable);
    }
    // 4. v2 指纹（签发与 planner 共用 `helper::card_fingerprint_v2`）：绑定实测题文、
    //    题数证据、lease_id、完整 scope 与 card/dismiss RuntimeId。不一致或序列化
    //    失败一律保守 ContentChanged（不沿用空指纹，不默认放行）。
    let computed = card_fingerprint_v2(
        &member.title,
        &member.options,
        1,
        &proof.lease_id,
        &proof.scope,
        &card.runtime_id,
        &card.dismiss_runtime_id,
    );
    if computed.as_deref() != Some(proof.card_fingerprint.as_str()) {
        return ClosePlan::Refuse(SkipReason::ContentChanged);
    }
    // 全部守卫通过：输出实测目标（scope 为已核对一致的绑定/快照共同作用域，
    // card/dismiss 取实测 RuntimeId），不在此执行任何副作用。
    ClosePlan::Invoke(InvokeTarget {
        scope: proof.scope.clone(),
        card_runtime_id: card.runtime_id.clone(),
        dismiss_runtime_id: card.dismiss_runtime_id.clone(),
    })
}
