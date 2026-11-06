//! Политика доступа к локальным файлам.
//!
//! Инструменты читают локальные файлы (загрузка вложений) и могут писать их (сохранение
//! ответа в `save_to`). Агент не должен через это читать или перезаписывать произвольные
//! файлы пользователя (`~/.ssh`, конфиги), поэтому любой путь должен лежать внутри одного из
//! разрешённых корней:
//!
//! - рабочий каталог процесса сервера (stdio-клиенты запускают сервер в каталоге проекта),
//!   если это не корень файловой системы и не домашний каталог — они слишком широкие;
//! - системный временный каталог;
//! - каталоги из `--allow-path`.
//!
//! Временный каталог разрешён, чтобы работали подготовленные там файлы и скачивание во
//! временные файлы; он общий для всех программ пользователя, поэтому это отмечено в `--help`
//! и документации.
//!
//! Корни и пути канонизируются (`canonicalize`), поэтому выход через `..` и символические
//! ссылки обнаруживается: проверяется реальный путь, а не то, как он записан.
//! Относительный путь разрешается относительно рабочего каталога сервера.
//!
//! Проверка возвращает канонический путь (для записи — канонический каталог и имя файла),
//! и исполнитель запросов открывает файл именно по нему, а после открытия сверяет, что путь
//! не подменили (`wiki_api::executor`, ошибка `LocalFileChanged`). Так закрыта гонка между
//! проверкой и открытием: замена файла или каталога символической ссылкой наружу после
//! проверки не даёт прочитать чужой файл и не даёт записать ответ за пределы корней.
//! Существующий файл (и символическая ссылка) при записи не перезаписывается: файл создаётся
//! с `create_new`.
//!
//! Сервер в HTTP-режиме работает не на машине агента, поэтому локальные файлы там выключены
//! целиком ([`FilePolicy::disabled`]): любой путь отклоняется, даже если аргумент с путём
//! дошёл до проверки.

use std::fmt;
use std::io;
use std::path::{Component, Path, PathBuf};

/// Ошибка проверки локального пути; текст предназначен для агента.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePolicyError(pub String);

impl fmt::Display for FilePolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for FilePolicyError {}

/// Разрешённые корни для локальных файлов.
#[derive(Debug, Clone)]
pub struct FilePolicy {
    /// Канонические пути корней без дубликатов.
    roots: Vec<PathBuf>,
    /// Каталог, относительно которого разрешаются относительные пути.
    base_dir: Option<PathBuf>,
    /// Локальные файлы выключены (HTTP-режим): любой путь отклоняется.
    disabled: bool,
}

/// Сообщение для агента, когда локальные файлы выключены.
pub const LOCAL_FILES_DISABLED: &str = "local files are not available: this server runs on \
    another machine (HTTP mode) and cannot read or write files on the agent's machine";

impl FilePolicy {
    /// Политика с корнями по умолчанию (рабочий каталог и временный каталог) и `extra`.
    ///
    /// Каталоги из `extra` должны существовать: опечатка в `--allow-path` — ошибка запуска.
    /// Недоступные корни по умолчанию пропускаются с предупреждением в лог.
    pub fn with_defaults(extra: &[PathBuf]) -> Result<Self, FilePolicyError> {
        let cwd = std::env::current_dir().ok();
        let home = home_dir().and_then(|home| home.canonicalize().ok());
        let mut defaults = Vec::new();
        if let Some(cwd) = cwd.as_ref().and_then(|cwd| cwd.canonicalize().ok()) {
            if cwd.parent().is_none() || Some(&cwd) == home.as_ref() {
                tracing::warn!(
                    cwd = %cwd.display(),
                    "рабочий каталог слишком широкий и не добавлен в разрешённые корни файлов"
                );
            } else {
                defaults.push(cwd);
            }
        }
        match std::env::temp_dir().canonicalize() {
            Ok(tmp) => defaults.push(tmp),
            Err(err) => tracing::warn!(%err, "временный каталог недоступен"),
        }

        let mut policy = Self::new(extra)?;
        for root in defaults {
            policy.push_root(root);
        }
        policy.base_dir = cwd;
        Ok(policy)
    }

    /// Политика только с указанными корнями (без корней по умолчанию).
    pub fn new(roots: &[PathBuf]) -> Result<Self, FilePolicyError> {
        let mut policy = Self {
            roots: Vec::new(),
            base_dir: None,
            disabled: false,
        };
        for root in roots {
            let canonical = root.canonicalize().map_err(|err| {
                FilePolicyError(format!(
                    "allowed path `{}` is not accessible: {err}",
                    root.display()
                ))
            })?;
            if !canonical.is_dir() {
                return Err(FilePolicyError(format!(
                    "allowed path `{}` is not a directory",
                    root.display()
                )));
            }
            policy.push_root(canonical);
        }
        Ok(policy)
    }

    /// Политика без локальных файлов (HTTP-режим): любой путь отклоняется.
    pub fn disabled() -> Self {
        Self {
            roots: Vec::new(),
            base_dir: None,
            disabled: true,
        }
    }

    /// Выключены ли локальные файлы ([`FilePolicy::disabled`]).
    pub fn is_disabled(&self) -> bool {
        self.disabled
    }

    /// Каталог для относительных путей. По умолчанию — рабочий каталог на момент создания.
    pub fn with_base_dir(mut self, base_dir: impl Into<PathBuf>) -> Self {
        self.base_dir = Some(base_dir.into());
        self
    }

    /// Канонические разрешённые корни.
    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    fn push_root(&mut self, root: PathBuf) {
        if !self.roots.contains(&root) {
            self.roots.push(root);
        }
    }

    /// Проверяет файл для чтения (загрузки). Возвращает канонический путь существующего
    /// обычного файла внутри разрешённого корня.
    pub fn check_read(&self, path: &str) -> Result<PathBuf, FilePolicyError> {
        self.ensure_enabled()?;
        let absolute = self.absolute(path)?;
        let canonical = absolute
            .canonicalize()
            .map_err(|err| self.io_error(path, &err))?;
        self.ensure_inside(path, &canonical)?;
        if !canonical.is_file() {
            return Err(FilePolicyError(format!("`{path}` is not a regular file")));
        }
        Ok(canonical)
    }

    /// Проверяет путь для записи (сохранения ответа). Каталог должен существовать и лежать
    /// внутри разрешённого корня, файла с таким именем быть не должно (в том числе
    /// символической ссылки). Возвращает путь вида `<канонический каталог>/<имя файла>`.
    pub fn check_write(&self, path: &str) -> Result<PathBuf, FilePolicyError> {
        self.ensure_enabled()?;
        let absolute = self.absolute(path)?;
        let file_name = match absolute.components().next_back() {
            Some(Component::Normal(name)) => name.to_owned(),
            _ => {
                return Err(FilePolicyError(format!(
                    "`{path}` must end with a file name"
                )));
            }
        };
        let parent = absolute
            .parent()
            .ok_or_else(|| FilePolicyError(format!("`{path}` has no parent directory")))?;
        let parent = parent
            .canonicalize()
            .map_err(|err| self.io_error(path, &err))?;
        self.ensure_inside(path, &parent)?;
        let target = parent.join(file_name);
        if target.symlink_metadata().is_ok() {
            return Err(FilePolicyError(format!(
                "`{path}` already exists; choose a new file name (existing files are never overwritten)"
            )));
        }
        Ok(target)
    }

    fn ensure_enabled(&self) -> Result<(), FilePolicyError> {
        if self.disabled {
            return Err(FilePolicyError(LOCAL_FILES_DISABLED.to_owned()));
        }
        Ok(())
    }

    fn absolute(&self, path: &str) -> Result<PathBuf, FilePolicyError> {
        if path.trim().is_empty() {
            return Err(FilePolicyError("file path is empty".into()));
        }
        let path = Path::new(path);
        if path.is_absolute() {
            return Ok(path.to_path_buf());
        }
        match &self.base_dir {
            Some(base) => Ok(base.join(path)),
            None => Err(FilePolicyError(format!(
                "`{}` is not an absolute path",
                path.display()
            ))),
        }
    }

    fn ensure_inside(&self, original: &str, canonical: &Path) -> Result<(), FilePolicyError> {
        if self.roots.iter().any(|root| canonical.starts_with(root)) {
            return Ok(());
        }
        Err(FilePolicyError(format!(
            "`{original}` is outside the allowed directories ({}); the user can allow more with \
             `--allow-path <dir>`",
            self.describe_roots()
        )))
    }

    fn io_error(&self, path: &str, err: &io::Error) -> FilePolicyError {
        let reason = match err.kind() {
            io::ErrorKind::NotFound => "does not exist".to_owned(),
            io::ErrorKind::PermissionDenied => "is not accessible (permission denied)".to_owned(),
            _ => format!("is not accessible: {err}"),
        };
        FilePolicyError(format!("`{path}` {reason}"))
    }

    /// Список корней для сообщений и инструкций сервера.
    pub fn describe_roots(&self) -> String {
        if self.disabled {
            return "none (local files are disabled)".to_owned();
        }
        if self.roots.is_empty() {
            return "none".to_owned();
        }
        self.roots
            .iter()
            .map(|root| display_path(root))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Путь для сообщений агенту. На Windows `canonicalize` возвращает verbatim-пути
/// (`\\?\C:\…`, `\\?\UNC\server\share\…`); агенту и пользователю показывается обычная
/// запись. Проверки по-прежнему идут по каноническим путям.
fn display_path(path: &Path) -> String {
    let text = path.display().to_string();
    if let Some(unc) = text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{unc}")
    } else if let Some(local) = text.strip_prefix(r"\\?\") {
        local.to_owned()
    } else {
        text
    }
}

/// Домашний каталог из окружения (`std::env::home_dir` помечен устаревшим в MSRV 1.85).
fn home_dir() -> Option<PathBuf> {
    ["HOME", "USERPROFILE"]
        .into_iter()
        .filter_map(std::env::var_os)
        .find(|value| !value.is_empty())
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    struct Fixture {
        _dir: tempfile::TempDir,
        root: PathBuf,
        outside: PathBuf,
        policy: FilePolicy,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let root = base.join("root");
        let outside = base.join("outside");
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(root.join("sub/file.txt"), "inside").unwrap();
        fs::write(outside.join("secret.txt"), "outside").unwrap();
        let policy = FilePolicy::new(std::slice::from_ref(&root))
            .unwrap()
            .with_base_dir(&root);
        Fixture {
            _dir: dir,
            root,
            outside,
            policy,
        }
    }

    fn s(path: &Path) -> String {
        path.display().to_string()
    }

    #[test]
    fn reads_files_inside_root() {
        let f = fixture();
        let file = f.root.join("sub/file.txt");
        assert_eq!(f.policy.check_read(&s(&file)).unwrap(), file);
        // Относительный путь — от base_dir.
        assert_eq!(f.policy.check_read("sub/file.txt").unwrap(), file);
        // `..`, который остаётся внутри корня, допустим.
        assert_eq!(f.policy.check_read("sub/../sub/file.txt").unwrap(), file);
    }

    #[test]
    fn rejects_escape_via_dotdot_and_relative() {
        let f = fixture();
        let escaped = f.root.join("../outside/secret.txt");
        let err = f.policy.check_read(&s(&escaped)).unwrap_err();
        assert!(err.0.contains("outside the allowed directories"), "{err}");
        let err = f.policy.check_read("../outside/secret.txt").unwrap_err();
        assert!(err.0.contains("outside the allowed directories"), "{err}");
        assert!(
            f.policy
                .check_read(&s(&f.outside.join("secret.txt")))
                .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_escape() {
        let f = fixture();
        let link = f.root.join("link.txt");
        std::os::unix::fs::symlink(f.outside.join("secret.txt"), &link).unwrap();
        let err = f.policy.check_read(&s(&link)).unwrap_err();
        assert!(err.0.contains("outside the allowed directories"), "{err}");

        let dir_link = f.root.join("out");
        std::os::unix::fs::symlink(&f.outside, &dir_link).unwrap();
        assert!(
            f.policy
                .check_read(&s(&dir_link.join("secret.txt")))
                .is_err()
        );
        assert!(f.policy.check_write(&s(&dir_link.join("new.txt"))).is_err());
        // Запись поверх существующей ссылки запрещена.
        assert!(f.policy.check_write(&s(&link)).is_err());
    }

    #[test]
    fn read_errors_are_clear() {
        let f = fixture();
        let err = f
            .policy
            .check_read(&s(&f.root.join("missing")))
            .unwrap_err();
        assert!(err.0.contains("does not exist"), "{err}");
        let err = f.policy.check_read(&s(&f.root.join("sub"))).unwrap_err();
        assert!(err.0.contains("not a regular file"), "{err}");
        assert!(f.policy.check_read("  ").is_err());
    }

    #[test]
    fn write_checks() {
        let f = fixture();
        let target = f.root.join("sub/new.bin");
        assert_eq!(f.policy.check_write(&s(&target)).unwrap(), target);
        assert_eq!(f.policy.check_write("sub/new.bin").unwrap(), target);
        let err = f
            .policy
            .check_write(&s(&f.root.join("sub/file.txt")))
            .unwrap_err();
        assert!(err.0.contains("already exists"), "{err}");
        assert!(f.policy.check_write(&s(&f.outside.join("x"))).is_err());
        assert!(f.policy.check_write(&s(&f.root.join("missing/x"))).is_err());
        assert!(f.policy.check_write(&s(&f.root.join("sub/.."))).is_err());
    }

    #[test]
    fn relative_paths_need_base_dir() {
        let f = fixture();
        let policy = FilePolicy::new(std::slice::from_ref(&f.root)).unwrap();
        let err = policy.check_read("sub/file.txt").unwrap_err();
        assert!(err.0.contains("not an absolute path"), "{err}");
    }

    #[test]
    fn extra_roots_must_exist() {
        let f = fixture();
        assert!(FilePolicy::new(&[f.root.join("missing")]).is_err());
        assert!(FilePolicy::new(&[f.root.join("sub/file.txt")]).is_err());
        let policy = FilePolicy::with_defaults(std::slice::from_ref(&f.outside)).unwrap();
        assert!(policy.roots().contains(&f.outside));
        let tmp = std::env::temp_dir().canonicalize().unwrap();
        assert!(policy.roots().contains(&tmp));
    }

    #[test]
    fn disabled_policy_rejects_every_path() {
        let f = fixture();
        let policy = FilePolicy::disabled();
        assert!(policy.is_disabled());
        assert!(!f.policy.is_disabled());
        let inside = s(&f.root.join("sub/file.txt"));
        for err in [
            policy.check_read(&inside).unwrap_err(),
            policy
                .check_write(&s(&f.root.join("sub/new.bin")))
                .unwrap_err(),
            policy.check_read("/etc/hosts").unwrap_err(),
        ] {
            assert_eq!(err.0, LOCAL_FILES_DISABLED);
        }
        assert!(policy.describe_roots().contains("disabled"));
    }

    #[test]
    fn display_path_hides_windows_verbatim_prefix() {
        assert_eq!(
            display_path(Path::new(r"\\?\C:\Users\me\project")),
            r"C:\Users\me\project"
        );
        assert_eq!(
            display_path(Path::new(r"\\?\UNC\server\share\dir")),
            r"\\server\share\dir"
        );
        assert_eq!(
            display_path(Path::new("/home/me/project")),
            "/home/me/project"
        );
    }
}
