; 兔维斯 NSIS 安装器钩子（2026-10-11 裁决：修「卸载不清自启残留」）：
; 卸载收尾时清掉本应用写入的两处 HKCU 注册表值（值名 = productName "Tuvis"，
; 即 tauri-plugin-autostart 的 app_name 落点）：
;   1. HKCU\...\CurrentVersion\Run\Tuvis          —— 自启命令行本体
;   2. HKCU\...\Explorer\StartupApproved\Run\Tuvis —— 任务管理器「启动应用」启用标记
; 没开过自启时 reg delete 返回非零——nsExec 把退出码压栈，必须 Pop 掉，不影响卸载继续。
; 边界如实申报：只清**执行卸载的这个用户**的 HKCU；其它 Windows 用户若也开过自启
; （多用户共用同机装法）不在此清理面内；HKLM 提权残留由应用内开关的写后验证负责。
!macro NSIS_HOOK_POSTUNINSTALL
  nsExec::Exec `reg delete "HKCU\Software\Microsoft\Windows\CurrentVersion\Run" /v "Tuvis" /f`
  Pop $0
  nsExec::Exec `reg delete "HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run" /v "Tuvis" /f`
  Pop $1
!macroend
