// SOURCE-MIRROR: ippoan/cdp-relay:rust/agent/build.rs
//! ビルド時にリリース tag (例: agent-dev-7) を binary に埋め込む (cc-webreview-ext#6)。
//! hello/pong の版表示と、将来の self-update が「自分が今どの版か」を知るために使う。
//!
//! 優先: 明示の CC_WEBREVIEW_RELEASE_TAG > GitHub Actions の GITHUB_REF_NAME。
//! ただし `agent-` prefix の tag だけ採用する (PR / branch ビルドの GITHUB_REF_NAME が
//! 紛れ込まないように)。どちらも無い / prefix 不一致なら埋め込まない (ローカルビルド扱い)。

fn main() {
    let tag = std::env::var("CC_WEBREVIEW_RELEASE_TAG")
        .ok()
        .or_else(|| std::env::var("GITHUB_REF_NAME").ok())
        .filter(|t| t.starts_with("agent-"));
    if let Some(tag) = tag {
        if !tag.is_empty() {
            println!("cargo:rustc-env=CC_WEBREVIEW_RELEASE_TAG={tag}");
        }
    }
    println!("cargo:rerun-if-env-changed=CC_WEBREVIEW_RELEASE_TAG");
    println!("cargo:rerun-if-env-changed=GITHUB_REF_NAME");

    // Windows target のみ: asInvoker マニフェストを埋め込み、Windows の installer-detection
    // による UAC 自動昇格を抑止する。target でガードするので Linux ビルドには無影響。
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        use embed_manifest::{embed_manifest, new_manifest};
        embed_manifest(new_manifest("CcWebreviewAgent")).expect("unable to embed manifest");
    }
}
