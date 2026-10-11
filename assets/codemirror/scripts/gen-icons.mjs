/**
 * Emits src/icons.rs from material-icon-theme's SVGs (MIT).
 *
 * Every row of the file tree gets a real, recognisable icon instead of one of
 * six generic shapes. The theme names files after language ids, not extensions
 * (`bash` does not exist, `console` does), so the maps below are curated by
 * hand — and anything unresolvable falls back to `file` / `folder` with a loud
 * report, so a theme upgrade that renames an icon shows up on the first build
 * rather than as a blank row.
 *
 * The whole `<svg>` element is copied through rather than reduced to paths:
 * 184 of the 1179 icons use `<g>`, `<circle>`, `<rect>`, gradients or `<use>`,
 * and a half-normalised icon is worse than none. Colours stay verbatim too —
 * `python.svg` is two-colour, so flattening it to one tint yields a blob.
 *
 * `<use>` and `url(#…)` are document-scoped, so every `id` is rewritten with
 * the icon's own name as a prefix. Without that, two visible rows that both
 * define `id="a"` would collide and render each other's shape.
 */
import { readFileSync, writeFileSync, readdirSync, existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join, resolve } from 'node:path';

const HERE = dirname(fileURLToPath(import.meta.url));
const PKG_ROOT = resolve(HERE, '..');
const ICONS_DIR = join(PKG_ROOT, 'node_modules', 'material-icon-theme', 'icons');
const OUT = resolve(HERE, '../../../src/icons.rs');

if (!existsSync(ICONS_DIR)) {
  console.error(`gen-icons: material-icon-theme is not installed (missing ${ICONS_DIR})`);
  console.error('gen-icons: run `npm install` in assets/codemirror first');
  process.exit(1);
}

// ── Curated maps ───────────────────────────────────────────────────────────
// `load_listing` already lowercases extensions, so EXT_ICON keys are lowercase.

const EXT_ICON = {
  rs: 'rust',
  py: 'python',
  js: 'javascript', mjs: 'javascript', cjs: 'javascript', jsx: 'javascript',
  ts: 'typescript', tsx: 'typescript', mts: 'typescript', cts: 'typescript',
  go: 'go',
  c: 'c', h: 'c',
  cc: 'cpp', cpp: 'cpp', cxx: 'cpp', hpp: 'cpp', hxx: 'cpp',
  cs: 'csharp',
  rb: 'ruby',
  php: 'php',
  swift: 'swift',
  kt: 'kotlin', kts: 'kotlin',
  scala: 'scala',
  pl: 'perl', pm: 'perl',
  lua: 'lua',
  r: 'r',
  dart: 'dart',
  ex: 'elixir', exs: 'elixir',
  hs: 'haskell', lhs: 'haskell',
  sh: 'console', bash: 'console', zsh: 'console', fish: 'console',
  bat: 'console', cmd: 'console',
  ps1: 'powershell', psm1: 'powershell', psd1: 'powershell',
  html: 'html', htm: 'html', xhtml: 'html',
  vue: 'vue', svelte: 'svelte', astro: 'astro',
  css: 'css', scss: 'css', sass: 'css', less: 'css',
  json: 'json', jsonc: 'json',
  yaml: 'yaml', yml: 'yaml',
  toml: 'toml',
  xml: 'xml', xsl: 'xml', xsd: 'xml', plist: 'xml',
  md: 'markdown', markdown: 'markdown', mdx: 'markdown',
  txt: 'document', rst: 'document', adoc: 'document', org: 'document',
  csv: 'document', tsv: 'document',
  log: 'log',
  sql: 'database', db: 'database', sqlite: 'database',
  png: 'image', jpg: 'image', jpeg: 'image', gif: 'image',
  webp: 'image', ico: 'image', bmp: 'image', avif: 'image', tiff: 'image',
  svg: 'svg',
  pdf: 'pdf',
  zip: 'zip', gz: 'zip', tar: 'zip', bz2: 'zip', xz: 'zip', '7z': 'zip', rar: 'zip',
  mp3: 'audio', wav: 'audio', flac: 'audio', ogg: 'audio', m4a: 'audio',
  mp4: 'video', mkv: 'video', avi: 'video', mov: 'video', webm: 'video',
  lock: 'lock',
  ini: 'settings', cfg: 'settings', conf: 'settings',
  env: 'settings', properties: 'settings',
  gradle: 'gradle', groovy: 'gradle',
  cmake: 'cmake', mk: 'makefile',
};

// Exact filename match, compared case-insensitively at runtime (Dockerfile,
// README.md, .gitignore …). Checked before the extension so `README.md` wins
// over the `md` rule.
const NAME_ICON = [
  ['dockerfile', 'docker'],
  ['docker-compose.yml', 'docker'],
  ['docker-compose.yaml', 'docker'],
  ['compose.yml', 'docker'],
  ['compose.yaml', 'docker'],
  ['makefile', 'makefile'],
  ['gnumakefile', 'makefile'],
  ['cmakelists.txt', 'cmake'],
  ['cargo.toml', 'toml'],
  ['cargo.lock', 'lock'],
  ['rust-toolchain', 'rust'],
  ['rust-toolchain.toml', 'rust'],
  ['package.json', 'npm'],
  ['package-lock.json', 'npm'],
  ['yarn.lock', 'lock'],
  ['pnpm-lock.yaml', 'pnpm'],
  ['readme', 'readme'],
  ['readme.md', 'readme'],
  ['readme.txt', 'readme'],
  ['license', 'license'],
  ['license.md', 'license'],
  ['licence', 'license'],
  ['copying', 'license'],
  ['changelog.md', 'markdown'],
  ['.gitignore', 'git'],
  ['.gitattributes', 'git'],
  ['.gitmodules', 'git'],
  ['.gitkeep', 'git'],
  ['.editorconfig', 'editorconfig'],
  ['.npmrc', 'npm'],
  ['.nvmrc', 'nodejs'],
  ['.eslintrc', 'eslint'],
  ['.eslintrc.json', 'eslint'],
  ['.eslintrc.js', 'eslint'],
  ['.prettierrc', 'prettier'],
  ['.prettierrc.json', 'prettier'],
  ['.bashrc', 'console'],
  ['.zshrc', 'console'],
  ['.profile', 'console'],
  ['.env', 'settings'],
  ['.env.local', 'settings'],
  ['.env.example', 'settings'],
  ['tsconfig.json', 'json'],
  ['webpack.config.js', 'webpack'],
  ['vite.config.js', 'webpack'],
  ['go.mod', 'go-mod'],
  ['go.sum', 'lock'],
  ['gemfile', 'ruby'],
  ['rakefile', 'ruby'],
  ['procfile', 'settings'],
  ['todo.md', 'markdown'],
];

// Directory name -> `folder-<name>` / `folder-<name>-open` pair. The right-hand
// side is the theme's own suffix, which does not always match the word we
// would have guessed (`tests` -> `folder-test`, `build` -> `folder-dist`).
const DIR_ICON = [
  ['src', 'src'],
  ['source', 'src'],
  ['assets', 'images'],
  ['images', 'images'],
  ['img', 'images'],
  ['icons', 'images'],
  ['public', 'public'],
  ['static', 'public'],
  ['docs', 'docs'],
  ['doc', 'docs'],
  ['test', 'test'],
  ['tests', 'test'],
  ['spec', 'test'],
  ['node_modules', 'node'],
  ['vendor', 'lib'],
  ['lib', 'lib'],
  ['config', 'config'],
  ['conf', 'config'],
  ['etc', 'config'],
  ['scripts', 'scripts'],
  ['script', 'scripts'],
  ['tools', 'tools'],
  ['build', 'dist'],
  ['dist', 'dist'],
  ['target', 'rust'],
  ['log', 'log'],
  ['logs', 'log'],
  ['tmp', 'temp'],
  ['temp', 'temp'],
  ['.github', 'github'],
  ['.git', 'git'],
  ['rust', 'rust'],
  ['python', 'python'],
  ['typescript', 'typescript'],
  ['javascript', 'javascript'],
  ['js', 'javascript'],
  ['css', 'css'],
  ['json', 'json'],
  ['yaml', 'config'],
  ['android', 'android'],
  ['ios', 'mobile'],
  ['components', 'components'],
  ['api', 'api'],
  ['db', 'database'],
  ['migrations', 'database'],
  ['locale', 'i18n'],
  ['locales', 'i18n'],
  ['i18n', 'i18n'],
  ['app', 'app'],
  ['archive', 'archive'],
  ['packages', 'packages'],
  ['vscode', 'vscode'],
];

// ── SVG loading ────────────────────────────────────────────────────────────

const available = new Set(
  readdirSync(ICONS_DIR)
    .filter((f) => f.endsWith('.svg') && !f.endsWith('.clone.svg'))
);
const unavailable = new Set();
const emitted = new Set();

const VIEWBOX_RE = /viewBox\s*=\s*"([^"]+)"/i;
const SVG_OPEN_RE = /<svg\b[^>]*>/i;
const SVG_CLOSE_RE = /<\/svg\s*>\s*$/i;
const ID_ATTR_RE = /(\s)id\s*=\s*"([^"]*)"/g;

/** Makes `id="a"` and every reference to it unique to this icon. */
function namespaceIds(svg, key) {
  const ids = [];
  let m;
  ID_ATTR_RE.lastIndex = 0;
  while ((m = ID_ATTR_RE.exec(svg)) !== null) ids.push(m[2]);
  if (ids.length === 0) return svg;
  for (const id of ids) {
    const unique = `${key}__${id}`;
    svg = svg
      .split(`id="${id}"`).join(`id="${unique}"`)
      .split(`href="#${id}"`).join(`href="#${unique}"`)
      .split(`url(#${id})`).join(`url(#${unique})`);
  }
  return svg;
}

/**
 * Returns the icon's complete `<svg>…</svg>` markup, or `null` when the file
 * is missing or has no `viewBox` (a viewBox we cannot guess would make the
 * paths render at the wrong scale).
 */
function loadIcon(target) {
  const file = target.endsWith('.svg') ? target : `${target}.svg`;
  if (!available.has(file)) return null;
  let svg = readFileSync(join(ICONS_DIR, file), 'utf8').trim();
  if (!SVG_OPEN_RE.test(svg) || !SVG_CLOSE_RE.test(svg)) return null;
  if (!VIEWBOX_RE.test(svg)) return null;

  const key = file.replace(/\.svg$/, '').replace(/[^a-zA-Z0-9]+/g, '_');
  svg = namespaceIds(svg, key);
  // XLink is an XML namespace: inside HTML, `href` is what actually resolves.
  svg = svg.replace(/\sxlink:href\s*=/g, ' href=');
  return svg;
}

function resolveIcon(target, report = true) {
  const svg = loadIcon(target);
  if (svg) {
    emitted.add(target.endsWith('.svg') ? target : `${target}.svg`);
    return svg;
  }
  if (report) unavailable.add(target);
  return null;
}

// ── Emission ───────────────────────────────────────────────────────────────

function rustStr(s) {
  return (
    '"' +
    s
      .replace(/\\/g, '\\\\')
      .replace(/"/g, '\\"')
      .replace(/\r/g, '\\r')
      .replace(/\n/g, '\\n')
      .replace(/\t/g, '\\t') +
    '"'
  );
}

const constName = (target) =>
  'I_' + target.replace(/\.svg$/, '').replace(/[^a-zA-Z0-9]+/g, '_').toUpperCase();

function emit() {
  const consts = []; // [rustConstName, svg]
  const seen = new Map(); // rustConstName -> svg

  const intern = (target) => {
    const svg = resolveIcon(target);
    if (!svg) return null;
    const name = constName(target);
    if (!seen.has(name)) {
      seen.set(name, svg);
      consts.push([name, svg]);
    }
    return name;
  };

  const extRows = [];
  for (const [ext, target] of Object.entries(EXT_ICON)) {
    const name = intern(target);
    if (name) extRows.push([ext, name]);
  }

  const nameRows = [];
  for (const [file, target] of NAME_ICON) {
    const name = intern(target);
    if (name) nameRows.push([file, name]);
  }

  const dirRows = [];
  const seenDirs = new Set();
  for (const [dir, target] of DIR_ICON) {
    if (seenDirs.has(target)) continue;
    seenDirs.add(target);
    const closed = intern(`folder-${target}`);
    if (!closed) continue;
    const opened = intern(`folder-${target}-open`);
    dirRows.push([target, closed, opened]);
  }

  const fallbackFolder = intern('folder');
  const fallbackOpen = intern('folder-open');
  const fallbackFile = intern('file');
  if (!fallbackFolder || !fallbackOpen || !fallbackFile) {
    console.error('gen-icons: folder, folder-open and file are required');
    process.exit(1);
  }

  const doc = [];
  doc.push(
    '// @generated by assets/codemirror/scripts/gen-icons.mjs — do not edit.'
  );
  doc.push('// Source: material-icon-theme (MIT)');
  doc.push('// https://github.com/material-extensions/vscode-material-icon-theme');
  doc.push('// Regenerate with `npm run gen:icons` in assets/codemirror.');
  doc.push('');

  const lookup = [];
  for (const [name, svg] of consts) {
    lookup.push(`static ${name}: &str = ${rustStr(svg)};`);
  }
  lookup.push('');
  lookup.push('static EXT_ICONS: &[(&str, &str)] = &[');
  for (const [ext, name] of extRows.sort((a, b) => a[0].localeCompare(b[0]))) {
    lookup.push(`    (${rustStr(ext)}, ${name}),`);
  }
  lookup.push('];');
  lookup.push('');
  lookup.push('static NAME_ICONS: &[(&str, &str)] = &[');
  for (const [file, name] of nameRows) {
    lookup.push(`    (${rustStr(file)}, ${name}),`);
  }
  lookup.push('];');
  lookup.push('');
  lookup.push(
    '/// (theme suffix, closed, opened) — one row per distinct suffix, so a'
  );
  lookup.push('/// directory alias reuses the same pair.');
  lookup.push(
    'static DIR_ICONS: &[(&str, &str, Option<&str>)] = &['
  );
  for (const [target, closed, opened] of dirRows) {
    const open = opened ? `Some(${opened})` : 'None';
    lookup.push(`    (${rustStr(target)}, ${closed}, ${open}),`);
  }
  lookup.push('];');
  lookup.push('');
  lookup.push(`static FOLDER_CLOSED: &str = ${fallbackFolder};`);
  lookup.push(`static FOLDER_OPEN: &str = ${fallbackOpen};`);
  lookup.push(`static FILE: &str = ${fallbackFile};`);
  lookup.push('');

  const fn = [];
  fn.push('/// Which icon an entry gets.');
  fn.push('///');
  fn.push('/// Returns the icon\'s complete `<svg>` markup, ready for');
  fn.push('/// `dangerous_inner_html`. The whole element is kept rather than');
  fn.push('/// flattened to `<path>`s because 184 icons rely on `<g>`, `<circle>`,');
  fn.push('/// `<rect>`, gradients or `<use>`, and on theme brand colours that are');
  fn.push('/// deliberately not overridden.');
  fn.push('///');
  fn.push('/// Order: exact file name (so `README.md` beats the `md` rule) →');
  fn.push('/// extension → directory → generic `file`. Directories prefer their');
  fn.push('/// paired `folder-<name>-open` variant while expanded.');
  fn.push('///');
  fn.push('/// Lookups allocate nothing: `eq_ignore_ascii_case` over static tables,');
  fn.push('/// because `ext` is already lowercased when the directory is read.');
  fn.push('pub fn icon_for(');
  fn.push('    name: &str,');
  fn.push('    ext: &str,');
  fn.push('    is_dir: bool,');
  fn.push('    expanded: bool,');
  fn.push(") -> &'static str {");
  fn.push('    if is_dir {');
  fn.push('        for (dir, closed, opened) in DIR_ICONS {');
  fn.push('            if dir.eq_ignore_ascii_case(name) {');
  fn.push('                return if expanded && opened.is_some() {');
  fn.push('                    opened.unwrap()');
  fn.push('                } else {');
  fn.push('                    closed');
  fn.push('                };');
  fn.push('            }');
  fn.push('        }');
  fn.push('        return if expanded { FOLDER_OPEN } else { FOLDER_CLOSED };');
  fn.push('    }');
  fn.push('');
  fn.push('    for (file_name, icon) in NAME_ICONS {');
  fn.push('        if file_name.eq_ignore_ascii_case(name) {');
  fn.push('            return icon;');
  fn.push('        }');
  fn.push('    }');
  fn.push('');
  fn.push('    for (extension, icon) in EXT_ICONS {');
  fn.push('        if *extension == ext {');
  fn.push('            return icon;');
  fn.push('        }');
  fn.push('    }');
  fn.push('');
  fn.push('    FILE');
  fn.push('}');
  fn.push('');

  writeFileSync(OUT, [...doc, ...lookup, ...fn].join('\n'), 'utf8');

  const missing = [...unavailable].filter(
    (t) => !emitted.has(t.endsWith('.svg') ? t : `${t}.svg`)
  );
  console.log(`gen-icons: wrote ${OUT}`);
  console.log(
    `gen-icons: ${consts.length} icons, ${extRows.length} extensions, ` +
      `${nameRows.length} file names, ${dirRows.length} directory rules`
  );
  if (missing.length) {
    console.warn(
      `gen-icons: ${missing.length} icon(s) unavailable — those rows fall back to file/folder:`
    );
    for (const m of missing.sort()) console.warn(`  - ${m}`);
  }
}

emit();
