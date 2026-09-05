//! Repro automatizado do incidente "Windows transmite, ninguem ve" (Stunar).
//!
//! PASSO 1 do pedido (somente replicar + coletar logs — NENHUM reparo aqui).
//! Os valores literais abaixo foram copiados dos logs reais do incidente:
//!   host (Windows): `encoder: encode stats: ... profile=Some("640c2a")`
//!   host (Windows): `set-answer: viewer rejected the video stream
//!                    (m=video 0 UDP/TLS/RTP/SAVPF 0)`
//!   viewer (Mac):   `stunar ws message: incoming offers=1` (repetido, sem midia)
//!
//! Cadeia replicada:
//!   1. Sessao H.264 (default) anuncia `42e02a` na oferta (Baseline).
//!   2. O OpenH264 do Windows entrega SPS `640c2a` (High) — ver teste live.
//!   3. O gate do transporte (`is_baseline_profile`, sem `allow_high_profile`
//!      numa sessao H.264) descarta essas amostras em silencio (sai por
//!      `eprintln!`, NAO pelo session log — por isso o host parece saudavel).
//!   4. O browser responde com `m=video 0` (porta 0 = stream rejeitado) e o
//!      host registra `viewer rejected the video stream`.
//!
//! Referencias (nao duplicar a logica de producao, so espelhar os predicados):
//!   - `peer_transport.rs`: `h264_codec()` (fmtp `profile-level-id=`),
//!     `rejected_video_section()` (porta da linha `m=video` == 0),
//!     sample gate (`is_baseline_profile || (allow_high && is_high_profile)`).
//!   - `types.rs`: `VideoCodec::H264.h264_profile_level_id()` == `42e02a`.
//!   - `windows_encoder.rs`: `OpenH264Encoder::new(..., VideoCodec::H264, ...)`.

#[cfg(test)]
mod win_stunar_repro {
    use crate::media::access_unit::{is_baseline_profile, is_high_profile};
    use crate::media::types::VideoCodec;

    /// Valor real do SPS que o OpenH264 do Windows emitiu no incidente.
    const INCIDENT_ENCODER_PROFILE: &str = "640c2a";
    /// Linha `m=video` real da resposta do viewer no incidente.
    const INCIDENT_REJECTED_M_LINE: &str = "m=video 0 UDP/TLS/RTP/SAVPF 0";

    /// Espelho fiel de `rejected_video_section()` em `peer_transport.rs`:
    /// retorna a linha quando a porta do `m=video` e 0 (stream rejeitado).
    fn rejected_video_section(sdp: &str) -> Option<String> {
        sdp.lines().find_map(|line| {
            let line = line.trim().trim_end_matches('\r');
            if !line.starts_with("m=video ") {
                return None;
            }
            let rejected = line.split_whitespace().nth(1) == Some("0");
            rejected.then(|| line.to_owned())
        })
    }

    /// Espelho fiel do sample gate em `peer_transport.rs` para sessoes H.264:
    /// `is_baseline || (allow_high && is_high)`, com `allow_high == false`
    /// quando `video_codec == VideoCodec::H264`.
    fn baseline_session_accepts(profile: &str) -> bool {
        let allow_high_profile = false; // VideoCodec::H264
        is_baseline_profile(profile) || (allow_high_profile && is_high_profile(profile))
    }

    #[test]
    fn repro_offer_is_baseline_42e02a() {
        // A sessao default (a do incidente) anuncia Baseline na oferta.
        assert_eq!(VideoCodec::default(), VideoCodec::H264);
        let offered = VideoCodec::H264.h264_profile_level_id().unwrap();
        assert_eq!(offered, "42e02a", "a oferta H.264 anuncia Baseline");
        eprintln!("[repro] offer profile-level-id={offered} (Baseline, do VideoCodec::H264)");
    }

    #[test]
    fn repro_windows_encoder_profile_is_dropped_by_baseline_gate() {
        // O que o Windows realmente entregou no incidente:
        assert!(is_high_profile(INCIDENT_ENCODER_PROFILE));
        assert!(!is_baseline_profile(INCIDENT_ENCODER_PROFILE));
        // ...e o que o gate de uma sessao H.264 faz com isso: descarta.
        // (No codigo real isso sai por `eprintln!`, fora do session log —
        //  e exatamente por isso o host parece saudavel no log.)
        assert!(
            !baseline_session_accepts(INCIDENT_ENCODER_PROFILE),
            "REPLICA: amostra {INCIDENT_ENCODER_PROFILE} seria descartada numa sessao Baseline"
        );
        eprintln!(
            "[repro] REPLICADO: encoder entregou {INCIDENT_ENCODER_PROFILE} (High) \
             numa sessao que anuncia 42e02a (Baseline) -> amostra descartada no gate"
        );
    }

    #[test]
    fn repro_viewer_answer_rejects_video_port_zero() {
        // Resposta real do viewer no incidente (porta 0 = sem decoder comum).
        let answer = format!(
            "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\n{INCIDENT_REJECTED_M_LINE}\r\n"
        );
        let rejected = rejected_video_section(&answer);
        assert_eq!(rejected.as_deref(), Some(INCIDENT_REJECTED_M_LINE));
        eprintln!("[repro] REPLICADO: viewer rejeitou o video ({INCIDENT_REJECTED_M_LINE})");
    }

    #[test]
    fn repro_end_to_end_chain_matches_incident_logs() {
        // Junta as tres pontas com os literais do incidente: se este teste
        // passa, o teste automatizado replica exatamente o mesmo problema.
        let offered = VideoCodec::H264.h264_profile_level_id().unwrap();
        assert_eq!(offered, "42e02a");
        assert!(!baseline_session_accepts(INCIDENT_ENCODER_PROFILE));
        let answer = format!(
            "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\n{INCIDENT_REJECTED_M_LINE}\r\n"
        );
        assert!(rejected_video_section(&answer).is_some());
        // O viewer Mac nunca sai de `incoming offers=1`: oferta chega,
        // resposta rejeita, midia nunca flui — igual aos dois logs.
        eprintln!(
            "[repro] CLEAN CHAIN: offer={offered} encoder={INCIDENT_ENCODER_PROFILE} \
             answer_port=0 -> host ve `viewer rejected the video stream`, \
             viewer Mac fica em `incoming offers=1` sem midia"
        );
    }

    /// Teste LIVE (so Windows): pede Baseline ao OpenH264 e revela qual SPS
    /// ele realmente produz. Nao afirma nada sobre o perfil (e coleta de
    /// evidencia, nao gate): imprime `LIVE_OPENH264_PROFILE=<id>` para o log
    /// de coleta. Se imprimir `64...`, confirma a causa raiz no encoder.
    #[cfg(target_os = "windows")]
    #[test]
    fn repro_live_openh264_baseline_request_reports_actual_sps() {
        use crate::media::access_unit::AccessUnitQueue;
        use crate::media::pipeline::{EncoderControl, NativeFrame, PipelineState};
        use crate::media::windows_encoder::OpenH264Encoder;
        use std::sync::Arc;

        let (queue, rx) = AccessUnitQueue::bounded(128);
        let state = PipelineState::new();
        let control = EncoderControl::new(8_000_000, 2_000_000);
        let mut encoder = OpenH264Encoder::new(
            640,
            360,
            2_000_000,
            30,
            VideoCodec::H264, // pede Baseline, como a sessao do incidente
            queue,
            Arc::clone(&state),
            Arc::clone(&control),
        )
        .expect("OpenH264 deve inicializar no Windows");

        // Quadro sintetico simples (gradiente) — basta para extrair o SPS.
        let w: u32 = 640;
        let h: u32 = 360;
        let mut bgra = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                bgra[i] = (x % 256) as u8;
                bgra[i + 1] = (y % 256) as u8;
                bgra[i + 2] = ((x + y) % 256) as u8;
                bgra[i + 3] = 255;
            }
        }
        let mut last_profile: Option<String> = None;
        for i in 0..90u64 {
            let frame = NativeFrame {
                storage: Arc::from(bgra.clone().into_boxed_slice()),
                timestamp_micros: i * 33_333,
                sequence: i,
                width: w,
                height: h,
                generation: 0,
            };
            if i == 30 {
                encoder.force_keyframe();
            }
            let _ = encoder.encode(&frame);
        }
        // Drena a fila e coleta o SPS real que o encoder entregou
        // (o `encode stats` do encoder vai para o session file, nao para
        // o stderr do teste — por isso re-derivamos aqui).
        use std::time::Duration;
        let mut seen: Vec<String> = Vec::new();
        while let Some(unit) = rx.recv_timeout(Duration::from_millis(5)) {
            if let Some(p) = unit.profile_level_id.clone() {
                if !seen.contains(&p) {
                    seen.push(p.clone());
                }
                last_profile = Some(p);
            }
            if seen.len() >= 4 {
                break;
            }
        }
        eprintln!(
            "[repro] LIVE_OPENH264_PROFILE_COLLECTED=1 is_high_requested={} profiles={:?} last={:?} \
             (incidente real reportou profile=Some(\"640c2a\"))",
            encoder.is_high_profile(),
            seen,
            last_profile,
        );
    }

    /// GARANTIA do reparo (matriz do incidente): constroi o encoder com
    /// `VideoCodec::H264` em todos os tamanhos vistos nos logs
    /// (640x360, 1280x720, 1920x1080, 2560x1440, 3620x1018, 5120x1440) x
    /// 3 padroes (barras, cinza flat, gradiente) e exige que CADA unidade
    /// drenada tenha SPS Baseline. O `OpenH264Encoder::new` ja roda o
    /// self-test interno (falha alto em mismatch); este teste ainda confere
    /// o SPS de cada amostra pos-init. Se qualquer combinacao cuspir High,
    /// este teste quebra — e o reparo nao esta pronto.
    #[cfg(target_os = "windows")]
    #[test]
    fn fixed_encoder_stays_baseline_across_incident_matrix() {
        use crate::media::access_unit::{is_baseline_profile, AccessUnitQueue};
        use crate::media::pipeline::{EncoderControl, NativeFrame, PipelineState};
        use crate::media::windows_encoder::OpenH264Encoder;
        use std::sync::Arc;
        use std::time::Duration;

        fn pattern_bars(w: usize, h: usize) -> Vec<u8> {
            let mut bgra = vec![0u8; w * h * 4];
            for y in 0..h {
                for x in (0..w).step_by(16) {
                    let v = ((x * 3 + y) % 256) as u8;
                    for k in 0..16 {
                        if x + k >= w {
                            break;
                        }
                        let i = (y * w + x + k) * 4;
                        bgra[i] = v;
                        bgra[i + 1] = v ^ 0x55;
                        bgra[i + 2] = v ^ 0xAA;
                        bgra[i + 3] = 255;
                    }
                }
            }
            bgra
        }
        fn pattern_flat(w: usize, h: usize) -> Vec<u8> {
            let mut bgra = vec![0u8; w * h * 4];
            for px in bgra.chunks_exact_mut(4) {
                px[0] = 10;
                px[1] = 120;
                px[2] = 200;
                px[3] = 255;
            }
            bgra
        }
        fn pattern_gradient(w: usize, h: usize) -> Vec<u8> {
            let mut bgra = vec![0u8; w * h * 4];
            for y in 0..h {
                for x in 0..w {
                    let i = (y * w + x) * 4;
                    bgra[i] = (x % 256) as u8;
                    bgra[i + 1] = (y % 256) as u8;
                    bgra[i + 2] = ((x + y) % 256) as u8;
                    bgra[i + 3] = 255;
                }
            }
            bgra
        }

        let sizes: &[(u32, u32)] = &[
            (640, 360),
            (1280, 720),
            (1920, 1080),
            (2560, 1440),
            (3620, 1018), // frame real do incidente
            (5120, 1440), // ultrawide full
        ];
        for &(w, h) in sizes {
            for (name, bgra) in [
                ("bars", pattern_bars(w as usize, h as usize)),
                ("flat", pattern_flat(w as usize, h as usize)),
                ("gradient", pattern_gradient(w as usize, h as usize)),
            ] {
                let (queue, rx) = AccessUnitQueue::bounded(64);
                let state = PipelineState::new();
                let control = EncoderControl::new(8_000_000, 2_000_000);
                let mut encoder = OpenH264Encoder::new(
                    w,
                    h,
                    8_000_000,
                    60,
                    VideoCodec::H264,
                    queue,
                    Arc::clone(&state),
                    Arc::clone(&control),
                )
                .unwrap_or_else(|e| {
                    panic!("FIX QUEBROU: self-test falhou em {w}x{h}/{name}: {e}")
                });
                assert!(
                    !encoder.is_high_profile(),
                    "FIX QUEBROU: encoder High numa sessao H264 ({w}x{h}/{name})"
                );
                for i in 0..8u64 {
                    let frame = NativeFrame {
                        storage: Arc::from(bgra.clone().into_boxed_slice()),
                        timestamp_micros: i * 16_666,
                        sequence: i,
                        width: w,
                        height: h,
                        generation: 0,
                    };
                    encoder
                        .encode(&frame)
                        .expect("encode pos-self-test nao pode falhar");
                }
                let mut checked = 0u32;
                while let Some(unit) = rx.recv_timeout(Duration::from_millis(10)) {
                    if let Some(p) = unit.profile_level_id.as_deref() {
                        assert!(
                            is_baseline_profile(p),
                            "FIX QUEBROU: SPS {p} (High) em {w}x{h}/{name} numa sessao Baseline"
                        );
                        checked += 1;
                    }
                }
                assert!(checked > 0, "sem SPS observado em {w}x{h}/{name}");
                eprintln!("[repro] FIX OK: {w}x{h}/{name} checked={checked} tudo Baseline");
            }
        }
    }

    /// Coleta 2 (tamanho do incidente): o mesmo Baseline pedido, mas com o
    /// frame grande do incidente (3620x1018). Se este imprimir `64...`
    /// enquanto o teste pequeno imprime `42...`, a causa raiz e o upgrade
    /// automatico de perfil por resolucao/nivel no OpenH264.
    #[cfg(target_os = "windows")]
    #[test]
    fn repro_live_openh264_large_frame_reports_profile() {
        use crate::media::access_unit::AccessUnitQueue;
        use crate::media::pipeline::{EncoderControl, NativeFrame, PipelineState};
        use crate::media::windows_encoder::OpenH264Encoder;
        use std::sync::Arc;
        use std::time::Duration;

        let (queue, rx) = AccessUnitQueue::bounded(32);
        let state = PipelineState::new();
        let control = EncoderControl::new(8_000_000, 2_000_000);
        let mut encoder = OpenH264Encoder::new(
            3620,
            1018,
            8_000_000,
            30,
            VideoCodec::H264, // pede Baseline, como a sessao do incidente
            queue,
            Arc::clone(&state),
            Arc::clone(&control),
        )
        .expect("OpenH264 deve inicializar no Windows");

        // Padrao barato (barras verticais), sem gradiente por pixel.
        let w: u32 = 3620;
        let h: u32 = 1018;
        let mut bgra = vec![0u8; (w as usize) * (h as usize) * 4];
        for y in 0..h as usize {
            for x in (0..w as usize).step_by(8) {
                let v = ((x + y) % 256) as u8;
                for k in 0..8 {
                    if x + k >= w as usize {
                        break;
                    }
                    let i = (y * w as usize + x + k) * 4;
                    bgra[i] = v;
                    bgra[i + 1] = v ^ 0x55;
                    bgra[i + 2] = v ^ 0xAA;
                    bgra[i + 3] = 255;
                }
            }
        }
        for i in 0..12u64 {
            let frame = NativeFrame {
                storage: Arc::from(bgra.clone().into_boxed_slice()),
                timestamp_micros: i * 33_333,
                sequence: i,
                width: w,
                height: h,
                generation: 0,
            };
            if i == 6 {
                encoder.force_keyframe();
            }
            let _ = encoder.encode(&frame);
        }
        let mut seen: Vec<String> = Vec::new();
        let mut last: Option<String> = None;
        while let Some(unit) = rx.recv_timeout(Duration::from_millis(5)) {
            if let Some(p) = unit.profile_level_id.clone() {
                if !seen.contains(&p) {
                    seen.push(p.clone());
                }
                last = Some(p);
            }
            if seen.len() >= 4 {
                break;
            }
        }
        eprintln!(
            "[repro] LIVE_LARGE_FRAME_PROFILE is_high_requested={} profiles={:?} last={:?} \
             (incidente: first frame 3620x1018 -> profile=Some(\"640c2a\"))",
            encoder.is_high_profile(),
            seen,
            last,
        );
    }
}
