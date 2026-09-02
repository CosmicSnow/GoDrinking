//! Windows Firewall helper for Direct/LAN host.
//!
//! Direct e LAN precisam de TCP inbound (GOLIVE2). Stunar só usa outbound
//! HTTPS/WSS, então não precisa de firewall.
//! No Windows o prompt do Firewall aparece automaticamente quando o app
//! tenta escutar sem regra — desde que a regra antiga tenha sido removida.
//! Este módulo só age no Windows e só para Direct/LAN.

use super::types::JoinMode;

#[cfg(target_os = "windows")]
use std::process::Command;

/// Se o join mode precisa de inbound TCP (firewall).
pub fn needs_firewall(join_mode: JoinMode) -> bool {
    matches!(join_mode, JoinMode::Direct | JoinMode::Lan)
}

#[cfg(not(target_os = "windows"))]
pub fn needs_firewall(_join_mode: JoinMode) -> bool {
    false
}

/// Tenta remover todas as regras antigas `goDrinking*`.
/// Não exige Admin para o app rodar: se falhar por falta de permissão,
/// retorna instrução manual via Configurações, sem bloquear o Start.
#[cfg(target_os = "windows")]
pub fn reset_firewall_rules() -> Result<String, String> {
    let ps = Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "try { Remove-NetFirewallRule -DisplayName 'goDrinking*' -ErrorAction Stop; 'ok' } catch { $_.Exception.Message }",
        ])
        .output()
        .map_err(|e| e.to_string())?;

    let out = String::from_utf8_lossy(&ps.stdout).trim().to_string();
    let err = String::from_utf8_lossy(&ps.stderr).trim().to_string();

    if out == "ok" || (ps.status.success() && err.is_empty()) {
        // Tenta limpar também via netsh (best-effort, silencioso)
        let _ = Command::new("netsh")
            .args(["advfirewall", "firewall", "delete", "rule", "name=goDrinking"])
            .output();
        Ok("regras goDrinking removidas (se existiam). Na próxima vez que iniciar Direct/LAN como Host, o Windows deve mostrar o prompt de Firewall — sem precisar rodar o app como Admin.".into())
    } else {
        // Falha típica sem Admin: Access is denied / precisa de privilégio
        let msg = if !err.is_empty() { err } else { out };
        if msg.to_lowercase().contains("denied")
            || msg.to_lowercase().contains("privilégio")
            || msg.to_lowercase().contains("privilege")
            || msg.to_lowercase().contains("admin")
        {
            Ok("Não foi possível remover automaticamente (sem permissão de Admin) — mas NÃO precisa rodar como Admin. Vá em Configurações > Firewall > Permitir app pelo firewall > Remova 'goDrinking' manualmente e na próxima vez que iniciar Direct/LAN como Host o Windows pedirá permissão. Stunar/LAN Viewer não precisam de firewall.".into())
        } else {
            Err(format!("reset falhou: {msg}"))
        }
    }
}

#[cfg(not(target_os = "windows"))]
pub fn reset_firewall_rules() -> Result<String, String> {
    Ok("firewall só no Windows".into())
}

/// Verifica se o executável atual já tem regra inbound Allow.
/// Não tenta elevar; só informa. O prompt do Windows aparece sozinho
/// quando o Host faz `bind()` sem regra.
#[cfg(target_os = "windows")]
pub fn check_firewall_status() -> String {
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "-".into());
    let out = Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "Get-NetFirewallRule -DisplayName 'goDrinking*' -ErrorAction SilentlyContinue | Format-Table DisplayName,Direction,Action,Enabled -AutoSize | Out-String",
        ])
        .output();
    match out {
        Ok(o) if o.status.success() => {
            let txt = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if txt.is_empty() || txt.contains("0") {
                format!("nenhuma regra goDrinking encontrada para {exe} — ao iniciar Direct/LAN o Windows deve pedir permissão (se o Firewall estiver ativo)")
            } else {
                format!("regras atuais:\n{txt}\nexe: {exe}")
            }
        }
        Ok(o) => {
            let e = String::from_utf8_lossy(&o.stderr).to_string();
            format!("check falhou: {e}")
        }
        Err(e) => format!("check erro: {e}"),
    }
}

#[cfg(not(target_os = "windows"))]
pub fn check_firewall_status() -> String {
    "firewall só no Windows".into()
}

/// Chamado ao criar sessão Direct/LAN. Não bloqueia o Start: dispara
/// a checagem em thread separada para não travar o `create_in_state`
/// (powershell `Get-NetFirewallRule` pode levar 1-5s no win e congelava o Host).
#[cfg(target_os = "windows")]
pub fn ensure_firewall_for_host(join_mode: JoinMode) {
    if !needs_firewall(join_mode) {
        return;
    }
    std::thread::spawn(|| {
        let status = check_firewall_status();
        super::logger::log("INFO", "firewall", &status);
    });
}

#[cfg(not(target_os = "windows"))]
pub fn ensure_firewall_for_host(_join_mode: JoinMode) {}
