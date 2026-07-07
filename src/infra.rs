//! `litek8s infra init` — scaffold a self-contained Terraform + deploy project.
//!
//! Writes a fixed tree of files (Terraform to provision litek8s nodes on AWS,
//! example manifests, and a `deploy.sh`) into a target directory so an app repo
//! can provision nodes and deploy without living inside the litek8s source tree.
//! Templates are embedded via `include_str!`; the project name is substituted
//! with a dumb `{{PROJECT_NAME}}` string replace.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

const PROJECT_NAME_PLACEHOLDER: &str = "{{PROJECT_NAME}}";

/// A file to scaffold: its relative path under the target dir and its template.
struct TemplateFile {
    /// Path relative to the scaffold directory.
    rel_path: &'static str,
    /// Embedded template contents.
    contents: &'static str,
}

const TEMPLATES: &[TemplateFile] = &[
    TemplateFile {
        rel_path: "terraform/main.tf",
        contents: include_str!("infra_templates/terraform/main.tf"),
    },
    TemplateFile {
        rel_path: "terraform/variables.tf",
        contents: include_str!("infra_templates/terraform/variables.tf"),
    },
    TemplateFile {
        rel_path: "terraform/outputs.tf",
        contents: include_str!("infra_templates/terraform/outputs.tf"),
    },
    TemplateFile {
        rel_path: "terraform/providers.tf",
        contents: include_str!("infra_templates/terraform/providers.tf"),
    },
    TemplateFile {
        rel_path: "terraform/versions.tf",
        contents: include_str!("infra_templates/terraform/versions.tf"),
    },
    TemplateFile {
        rel_path: "terraform/userdata.sh",
        contents: include_str!("infra_templates/terraform/userdata.sh"),
    },
    TemplateFile {
        rel_path: "terraform/.gitignore",
        contents: include_str!("infra_templates/terraform/gitignore"),
    },
    TemplateFile {
        rel_path: "terraform/terraform.tfvars.example",
        contents: include_str!("infra_templates/terraform/terraform.tfvars.example"),
    },
    TemplateFile {
        rel_path: "deploy/app.yaml",
        contents: include_str!("infra_templates/deploy/app.yaml"),
    },
    TemplateFile {
        rel_path: "deploy/service.yaml",
        contents: include_str!("infra_templates/deploy/service.yaml"),
    },
    TemplateFile {
        rel_path: "deploy.sh",
        contents: include_str!("infra_templates/deploy.sh"),
    },
    TemplateFile {
        rel_path: "README.md",
        contents: include_str!("infra_templates/README.md"),
    },
];

/// Files that should be executable once written.
const EXECUTABLE: &[&str] = &["deploy.sh", "terraform/userdata.sh"];

/// Scaffold the Terraform + deploy project into `dir`.
///
/// Substitutes `{{PROJECT_NAME}}` with `name`. Existing files are skipped unless
/// `force` is set. Parent directories are created as needed.
pub fn scaffold(dir: &Path, name: &str, force: bool) -> Result<()> {
    fs::create_dir_all(dir)
        .with_context(|| format!("failed to create scaffold directory {}", dir.display()))?;

    for template in TEMPLATES {
        let target = dir.join(template.rel_path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create directory {}", parent.display()))?;
        }

        if target.exists() && !force {
            eprintln!("skip (exists): {}", target.display());
            continue;
        }

        let rendered = template.contents.replace(PROJECT_NAME_PLACEHOLDER, name);
        fs::write(&target, rendered)
            .with_context(|| format!("failed to write {}", target.display()))?;

        if EXECUTABLE.contains(&template.rel_path) {
            make_executable(&target)?;
        }

        eprintln!("wrote: {}", target.display());
    }

    print_next_steps(dir);
    Ok(())
}

/// Mark a scaffolded script as executable (best-effort, unix only).
#[cfg(unix)]
fn make_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path)
        .with_context(|| format!("failed to stat {}", path.display()))?
        .permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms)
        .with_context(|| format!("failed to chmod {}", path.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<()> {
    Ok(())
}

fn print_next_steps(dir: &Path) {
    let dir = dir.display();
    println!();
    println!("Scaffolded a litek8s Terraform + deploy project into {dir}");
    println!();
    println!("Next steps:");
    println!("  cd {dir}/terraform");
    println!("  cp terraform.tfvars.example terraform.tfvars   # edit profile/region/nodes");
    println!("  terraform init");
    println!("  terraform plan");
    println!("  terraform apply");
    println!("  cd ..");
    println!("  ./deploy.sh                                    # register nodes + apply manifests");
    println!();
    println!("deploy.sh runs `litek8s node add ...` for each node, then `litek8s apply`.");
    println!("See README.md for the full flow.");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A unique temp directory for a test, cleaned up on drop.
    struct TempScaffold {
        path: std::path::PathBuf,
    }

    impl TempScaffold {
        fn new() -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "litek8s-infra-test-{}-{}",
                std::process::id(),
                n
            ));
            let _ = fs::remove_dir_all(&path);
            Self { path }
        }
    }

    impl Drop for TempScaffold {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn scaffolds_the_expected_tree() {
        let tmp = TempScaffold::new();
        scaffold(&tmp.path, "demo-app", false).expect("scaffold should succeed");

        for rel in [
            "terraform/main.tf",
            "terraform/variables.tf",
            "terraform/outputs.tf",
            "terraform/providers.tf",
            "terraform/versions.tf",
            "terraform/userdata.sh",
            "terraform/.gitignore",
            "terraform/terraform.tfvars.example",
            "deploy/app.yaml",
            "deploy/service.yaml",
            "deploy.sh",
            "README.md",
        ] {
            assert!(
                tmp.path.join(rel).exists(),
                "expected scaffolded file {rel} to exist"
            );
        }
    }

    #[test]
    fn substitutes_project_name_everywhere() {
        let tmp = TempScaffold::new();
        let name = "acme-service";
        scaffold(&tmp.path, name, false).expect("scaffold should succeed");

        // The name lands in files that carry the placeholder.
        let tfvars = fs::read_to_string(tmp.path.join("terraform/terraform.tfvars.example"))
            .expect("read tfvars.example");
        assert!(
            tfvars.contains(name),
            "tfvars.example should contain the project name"
        );

        // No generated file may still carry a literal placeholder.
        assert_no_placeholder_remaining(&tmp.path);
    }

    fn assert_no_placeholder_remaining(dir: &Path) {
        for entry in fs::read_dir(dir).expect("read_dir") {
            let entry = entry.expect("dir entry");
            let path = entry.path();
            if path.is_dir() {
                assert_no_placeholder_remaining(&path);
            } else {
                let contents = fs::read_to_string(&path).expect("read file");
                assert!(
                    !contents.contains(PROJECT_NAME_PLACEHOLDER),
                    "{} still contains {PROJECT_NAME_PLACEHOLDER}",
                    path.display()
                );
            }
        }
    }

    #[test]
    fn force_false_skips_existing_and_force_true_overwrites() {
        let tmp = TempScaffold::new();
        let readme = tmp.path.join("README.md");
        fs::create_dir_all(&tmp.path).expect("mkdir");
        fs::write(&readme, "SENTINEL").expect("write sentinel");

        // force = false: the existing sentinel is left untouched.
        scaffold(&tmp.path, "demo", false).expect("scaffold should succeed");
        assert_eq!(
            fs::read_to_string(&readme).expect("read readme"),
            "SENTINEL",
            "force=false should skip an existing file"
        );

        // force = true: the sentinel is overwritten with the template.
        scaffold(&tmp.path, "demo", true).expect("scaffold with force should succeed");
        assert_ne!(
            fs::read_to_string(&readme).expect("read readme"),
            "SENTINEL",
            "force=true should overwrite an existing file"
        );
    }

    #[test]
    fn generated_manifests_parse() {
        let tmp = TempScaffold::new();
        scaffold(&tmp.path, "demo-app", false).expect("scaffold should succeed");

        let app = fs::read_to_string(tmp.path.join("deploy/app.yaml")).expect("read app.yaml");
        crate::manifest::ParsedManifest::parse(&app).expect("app.yaml should be a valid manifest");

        let svc =
            fs::read_to_string(tmp.path.join("deploy/service.yaml")).expect("read service.yaml");
        crate::manifest::ParsedManifest::parse(&svc)
            .expect("service.yaml should be a valid manifest");
    }
}
