//! Dev harness: run the statement importer against a scratch vault.
//! `cargo run -p trove-core --example csv_import_check -- <vault> <csv> [account-id|--new <name>]`

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [vault_root, csv, rest @ ..] = args.as_slice() else {
        anyhow::bail!("usage: csv_import_check <vault> <csv> [account-id|--new <name>]");
    };
    let vault = trove_core::Vault::open_or_create(vault_root.into())?;
    let (account, new_name) = match rest {
        [flag, name] if flag == "--new" => (None, Some(name.as_str())),
        [id] => (Some(id.as_str()), None),
        _ => (None, None),
    };
    let stats = vault.finance_import_csv(std::path::Path::new(csv), account, new_name)?;
    println!("{}", serde_json::to_string_pretty(&stats)?);
    Ok(())
}
