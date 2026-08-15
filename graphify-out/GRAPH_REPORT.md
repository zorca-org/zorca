# Graph Report - zorca  (2026-08-12)

## Corpus Check
- 134 files · ~57,755 words
- Verdict: corpus is large enough that graph structure adds value.

## Summary
- 727 nodes · 2256 edges · 99 communities (55 shown, 44 thin omitted)
- Extraction: 98% EXTRACTED · 2% INFERRED · 0% AMBIGUOUS · INFERRED: 37 edges (avg confidence: 0.91)
- Token cost: 0 input · 0 output

## Graph Freshness
- Built from commit: `6689c718`
- Run `git rev-parse HEAD` and compare to check if the graph is stale.
- Run `graphify update .` after code changes (no API cost).

## Community Hubs (Navigation)
- Core Zed Application
- GPUI Project Infrastructure
- Metal Rendering Shaders
- Build and Packaging Tools
- Renovate Dependency Configuration
- Prettier Server Protocol
- macOS Bundle Pipeline
- Cargo Timing Analysis
- Linux Bundle Pipeline
- Metal Debugging Tools
- Terminal Color Diagnostics
- Highlight Analysis
- License Compliance Checks
- sccache Setup
- Uninstallation Scripts
- Flatpak Bundling
- Linux Installation
- Blob Store Uploads
- macOS Icon Verification
- Docker Build Pipeline
- License Generation
- Installation Script
- Linux Packaging Script
- Color SVG Example
- Sandbox HTTP Proxy
- Extension Test API
- Bind Source TOCTOU Test
- True Color Demo
- Zed Launcher Script
- Icon Generation
- Keymap Validation
- TODO Validation
- Target Directory Cleanup
- Clippy Runner
- Crate Dependency Graph
- CLI Debugging
- WASI SDK Download
- Dependency Installer
- License CSV Generation
- Terms RTF Generation
- Crate Version Lookup
- CMake Installation
- MITM Proxy Script
- New Crate Generator
- Prettier Runner
- Remote Server Script
- Shell Script Validation
- JSON Schema Update
- Single Lint Runner
- Auto Update Helper
- Explorer Command Injection
- GPUI Linux Backend
- Media Module
- Windows Resources
- Build Task Runner
- Dragon SVG Example
- Release Automation
- Graphify Update Hook
- Build Speed Analysis
- Bind Mount Security
- Sandbox Security Architecture
- ZOrca Brand System
- ZOrca Product Website
- Agent Skills System
- GPUI Documentation
- Vim Test Infrastructure
- Custom Dylint Tooling
- Dylint Creation Workflow
- Extension API Ecosystem
- Issue Reporting Workflow
- Conversation Summary Prompts
- Icon Design System
- Extension API Changes
- Arrow Circle SVG
- Phantom Project Investigation
- Agent Contribution Rules
- GitHub Pages Deployment
- Contributor Conduct
- Breakpoint Management
- Commit Message Guidelines
- Buf Lint Configuration
- pre-commit
- Database Query Workflow
- Logging Configuration
- Documentation Assets

## God Nodes (most connected - your core abstractions)
1. `gpui` - 137 edges
2. `zed` - 116 edges
3. `util` - 101 edges
4. `project` - 87 edges
5. `workspace` - 87 edges
6. `settings` - 81 edges
7. `editor` - 77 edges
8. `collections` - 74 edges
9. `language` - 70 edges
10. `ui` - 64 edges

## Surprising Connections (you probably didn't know these)
- `GPUI Executor Timer Preference` --semantically_similar_to--> `Rust and GPUI Agent Rules`  [INFERRED] [semantically similar]
  .agents/skills/gpui-test/SKILL.md → AGENTS.md
- `Pull Request Hygiene` --semantically_similar_to--> `Release Notes Format`  [INFERRED] [semantically similar]
  AGENTS.md → .github/pull_request_template.md
- `ZOrca Contribution Guide` --semantically_similar_to--> `Release Notes Format`  [INFERRED] [semantically similar]
  CONTRIBUTING.md → .github/pull_request_template.md
- `Contribution Validation Workflow` --semantically_similar_to--> `Pull Request Checks Workflow`  [INFERRED] [semantically similar]
  CONTRIBUTING.md → .github/workflows/pr-checks.yml
- `ZOrca Brand Commitments` --semantically_similar_to--> `ZOrca Branding`  [INFERRED] [semantically similar]
  website/PRODUCT.md → docs/branding/README.md

## Import Cycles
- None detected.

## Hyperedges (group relationships)
- **Cross-Platform Build and Release Automation** — _github_workflows_pr_checks_linux_checks, _github_workflows_pr_checks_windows_checks, _github_workflows_release_cross_platform_packaging, _github_workflows_zorca_ci_scheduled_cross_platform_builds [INFERRED 0.95]
- **Upstream Zed Routing and Fork Policy** — _github_issue_template_bug_report_upstream_zed_issue_routing, contributing_upstream_contribution_boundary, readme_independent_zed_hard_fork, security_security_policy [INFERRED 0.85]
- **Agent Skill Lifecycle** — crates_agent_skills_readme_agent_skills_design, crates_agent_skills_readme_progressive_disclosure, crates_agent_skills_readme_activation_and_security, crates_agent_skills_builtin_create_skill_skill_creating_a_zed_agent_skill [INFERRED 0.95]
- **GPUI Application Model** — crates_gpui_readme_application, crates_gpui_readme_entity_state_management, crates_gpui_readme_views, crates_gpui_readme_elements, crates_gpui_docs_contexts_gpui_contexts, crates_gpui_docs_key_dispatch_key_dispatch [EXTRACTED 1.00]
- **Linux Sandbox Defense Chain** — crates_sandbox_readme_linux_bind_mount_toctou, crates_sandbox_readme_file_descriptor_validation, crates_sandbox_readme_seccomp_ipc_protection, crates_sandbox_readme_network_proxy_architecture, crates_sandbox_readme_wsl_sandbox_helper [EXTRACTED 1.00]
- **ZOrca Public Product Truth** — website_product_zorca_product, website_design_website_design_system, website_index_zorca_website, docs_branding_readme_zorca_branding, docs_readme_zorca_product_documentation [INFERRED 0.95]
- **Three-by-Three Circle Arrangement** — crates_gpui_examples_image_color_dark_center_circle, crates_gpui_examples_image_color_green_top_left_circle, crates_gpui_examples_image_color_red_top_center_circle, crates_gpui_examples_image_color_amber_top_right_circle, crates_gpui_examples_image_color_cyan_middle_right_circle, crates_gpui_examples_image_color_translucent_blue_middle_left_circle, crates_gpui_examples_image_color_purple_bottom_center_circle, crates_gpui_examples_image_color_magenta_bottom_right_circle, crates_gpui_examples_image_color_translucent_rose_bottom_left_circle [INFERRED 0.95]
- **Dragon Anatomical Composition** — crates_gpui_examples_svg_dragon_dragon_head, crates_gpui_examples_svg_dragon_coiled_dragon_body, crates_gpui_examples_svg_dragon_horns_and_antlers, crates_gpui_examples_svg_dragon_flame_like_mane, crates_gpui_examples_svg_dragon_fangs_and_tusks [INFERRED 0.95]

## Communities (99 total, 44 thin omitted)

### Community 0 - "Core Zed Application"
Cohesion: 0.15
Nodes (71): breadcrumbs, buffer_diff, clock, command_palette, csv_preview, db, derive_refineable, diagnostics (+63 more)

### Community 1 - "GPUI Project Infrastructure"
Cohesion: 0.12
Nodes (109): activity_indicator, agent_settings, agent_skills, agent_workspaces, anthropic, askpass, assets, audio (+101 more)

### Community 2 - "Metal Rendering Shaders"
Cohesion: 0.07
Nodes (84): AtlasTile, Background, Bounds_ScaledPixels, constant, Corners_ScaledPixels, blur_along_x(), corner_dash_velocity(), dash_alpha() (+76 more)

### Community 3 - "Build and Packaging Tools"
Cohesion: 0.08
Nodes (22): BuildRemoteServer(), BuildZorcaAndItsFriends(), aliasBlockRegex(), CARGO_ALIASES, detectShell(), expandAlias(), findLatestTimingFile(), findSubcommand() (+14 more)

### Community 4 - "Renovate Dependency Configuration"
Cohesion: 0.12
Nodes (16): after 3pm on Wednesday, config:recommended, :dependencyDashboardApproval, group:serdeMonorepo, helpers:pinGitHubActionDigests, **/node_modules/**, :semanticCommitsDisabled, :separateMultipleMajorReleases (+8 more)

### Community 5 - "Prettier Server Protocol"
Cohesion: 0.24
Nodes (11): { Buffer }, fs, handleBuffer(), handleMessage(), makeError(), { once }, path, Prettier (+3 more)

### Community 6 - "macOS Bundle Pipeline"
Cohesion: 0.21
Nodes (12): bundle-mac script, CARGO_BUNDLE_SKIP_BUILD, CXXFLAGS, download_and_unpack(), download_git(), help_info(), RELEASE_CHANNEL, restore_bundle_manifest() (+4 more)

### Community 7 - "Cargo Timing Analysis"
Cohesion: 0.26
Nodes (11): analyzeTimings(), args, extractUnitData(), formatTime(), formatUnit(), fs, getZedDataDir(), os (+3 more)

### Community 8 - "Linux Bundle Pipeline"
Cohesion: 0.20
Nodes (9): bundle-linux script, APP_ARGS, APP_CLI, APP_ICON, CC, DO_STARTUP_NOTIFY, help_info(), RELEASE_VERSION (+1 more)

### Community 9 - "Metal Debugging Tools"
Cohesion: 0.18
Nodes (10): metal-debug script, DYLD_INSERT_LIBRARIES, DYLD_LIBRARY_PATH, DYMTL_TOOLS_DYLIB_PATH, GPUProfilerEnabled, GPUTOOLS_LOAD_GTMTLCAPTURE, LD_LIBRARY_PATH, METAL_DEBUG_ERROR_MODE (+2 more)

### Community 10 - "Terminal Color Diagnostics"
Cohesion: 0.60
Nodes (4): print_blocks(), print_colour(), print_run(), print256color.sh script

### Community 11 - "Highlight Analysis"
Cohesion: 0.60
Nodes (5): count_instances(), find_highlight_files(), main(), parse_arguments(), print_instances()

### Community 12 - "License Compliance Checks"
Cohesion: 0.60
Nodes (5): check-licenses script, check_license(), check_manifest_for_agpl(), check_no_agpl_license_file(), check_symlink_target()

### Community 13 - "sccache Setup"
Cohesion: 0.70
Nodes (4): setup-sccache script, configure_sccache(), install_sccache(), show_config()

### Community 14 - "Uninstallation Scripts"
Cohesion: 0.80
Nodes (4): linux(), macos(), prompt_remove_preferences(), uninstall.sh script

### Community 15 - "Flatpak Bundling"
Cohesion: 0.50
Nodes (3): bundle-flatpak script, ARCHIVE, CHANNEL

### Community 16 - "Linux Installation"
Cohesion: 0.50
Nodes (3): install-linux script, ZORCA_BUNDLE_PATH, ZORCA_CHANNEL

### Community 17 - "Blob Store Uploads"
Cohesion: 0.83
Nodes (3): UploadToBlobStore(), UploadToBlobStorePublic(), UploadToBlobStoreWithACL()

### Community 19 - "macOS Icon Verification"
Cohesion: 0.83
Nodes (3): verify-macos-document-icon script, fail(), usage()

### Community 26 - "Color SVG Example"
Cohesion: 0.20
Nodes (10): Amber Top-Right Circle, Cyan Middle-Right Circle, Dark Center Circle, Green Top-Left Circle, Magenta Bottom-Right Circle, Multicolor Grip Circle Grid, Purple Bottom-Center Circle, Red Top-Center Circle (+2 more)

### Community 69 - "Dragon SVG Example"
Cohesion: 0.20
Nodes (10): CC BY-SA 3.0 License, Coiled Dragon Body, Dragon Clip Art, Dragon Head, Fangs and Tusks, Flame-Like Mane, Flat Vector Illustration, Horns and Antlers (+2 more)

### Community 70 - "Release Automation"
Cohesion: 0.33
Nodes (7): Cross-Platform Packaging, GitHub Release Publication, Release Tag Validation, Release Workflow, Rolling Nightly Release, Scheduled Cross-Platform Builds, ZOrca CI Workflow

### Community 72 - "Build Speed Analysis"
Cohesion: 0.29
Nodes (7): Build Identity Invalidation, Build Optimization Order, Application Build Speed Analysis, Release Packaging Bottleneck, Redundant Release Debuginfo, Repeated License Generation, Serial Application and Remote-Server Builds

### Community 73 - "Bind Mount Security"
Cohesion: 0.50
Nodes (4): File-Descriptor Bind Validation, HostFilesystemLocation, Linux Bind-Mount TOCTOU Attack, WSL Sandbox Helper

### Community 74 - "Sandbox Security Architecture"
Cohesion: 0.25
Nodes (8): Hostile-Code Security Model, macOS Seatbelt Policy, Sandbox Network Proxy Architecture, Platform-Specific Sandbox Implementations, Sandbox, SandboxPolicy, SandboxFilesystemLocation, Seccomp IPC-Socket Protection

### Community 75 - "ZOrca Brand System"
Cohesion: 0.14
Nodes (15): ZOrca Brand Palette, Distinct Product Identity, Application Icon Generation, ZOrca Logo Source of Truth, ZOrca Branding, Website Accessibility and Motion, Website Color System, Product-Led Design Direction (+7 more)

### Community 76 - "ZOrca Product Website"
Cohesion: 0.13
Nodes (18): Current-versus-Planned Content Separation, Current Capabilities Section, ZOrca Website Hero, Source Installation Section, ZOrca, Zed, and Orca Comparison, Coming Soon Roadmap, Zed Foundation Section, ZOrca Public Website (+10 more)

### Community 77 - "Agent Skills System"
Cohesion: 0.24
Nodes (10): Creating a Zed Agent Skill, Skill File Format, Skill Scope Selection, Skill Activation and Security, Agent Skills Design, Agent Skills Specification, Flat Two-Scope Skill Discovery, Skill Progressive Disclosure (+2 more)

### Community 78 - "GPUI Documentation"
Cohesion: 0.07
Nodes (28): App Context, AsyncApp and AsyncWindowContext, Context<T>, Entity<T>, GPUI Contexts, TestAppContext, Window, Keyboard Action (+20 more)

### Community 79 - "Vim Test Infrastructure"
Cohesion: 0.50
Nodes (4): Neovim Result Cache, NeovimBackedTestContext, VimTestContext, Zed Vim Mode

### Community 80 - "Custom Dylint Tooling"
Cohesion: 0.33
Nodes (6): Cargo Fingerprint Clean, Current Custom Lints, Dylint Library, --force-warn Requirement, Pinned Nightly Toolchain, single-lint Helper

### Community 81 - "Dylint Creation Workflow"
Cohesion: 0.67
Nodes (3): Clippy Redundancy Check, Dylint Creation Rules, Lint UI Test Requirement

### Community 82 - "Extension API Ecosystem"
Cohesion: 0.20
Nodes (10): Development Extension Installation, Extension API Compatibility, Extension Manifest, Extension Trait, WebAssembly Extension Packaging, Zed Rust Extension API, Extension Test Fixture, Zed Extension Ecosystem Compatibility (+2 more)

### Community 83 - "Issue Reporting Workflow"
Cohesion: 0.67
Nodes (3): ZOrca Bug Report, Upstream Zed Issue Routing, Issue Template Configuration

### Community 84 - "Conversation Summary Prompts"
Cohesion: 0.67
Nodes (3): Conversation Compaction Handoff, Detailed Conversation Summary, Conversation Title Generation

### Community 85 - "Icon Design System"
Cohesion: 0.40
Nodes (5): Icon Contribution Flow, Icon Design Guidelines, Lucide, Phosphor, Zed Icons

### Community 86 - "Extension API Changes"
Cohesion: 0.67
Nodes (3): Batched Breaking Release, Extension API Breaking Changes, SlashCommand.menu_text Rename

### Community 87 - "Arrow Circle SVG"
Cohesion: 0.67
Nodes (3): Circular Opposing Arrows Icon, Circular Opposing Arrows, Refresh or Synchronization

### Community 88 - "Phantom Project Investigation"
Cohesion: 0.40
Nodes (4): Answer, Outcome, Q: Deeply investigate why renaming the worktree is creating a phantom project. We should reproduce it with tests and fix, Source Nodes

### Community 89 - "Agent Contribution Rules"
Cohesion: 0.06
Nodes (35): Deterministic GPUI Test Scheduler, GPUI Executor Timer Preference, GPUI Test Debugging, Parking Failure Diagnostics, Dynamic Channel Branch Mapping, Prerequisite-Aware Conflict Resolution, Canonical Cherry-Pick Script, Zed Cherry-Pick Procedure (+27 more)

### Community 100 - "Documentation Assets"
Cohesion: 0.67
Nodes (3): Project Assets, Zed Feature Documentation, ZOrca Product Documentation

## Ambiguous Edges - Review These
- `Circular Opposing Arrows` → `Refresh or Synchronization`  [AMBIGUOUS]
  crates/gpui/examples/image/arrow_circle.svg · relation: conceptually_related_to

## Knowledge Gaps
- **150 isolated node(s):** `solid`, `color0`, `color1`, `tile_position`, `clip_distance` (+145 more)
  These have ≤1 connection - possible missing edges or undocumented components.
- **44 thin communities (<3 nodes) omitted from report** — run `graphify query` to explore isolated nodes.

## Work-memory lessons

**Preferred sources** — corroborated by past sessions; start here.
- `terminal_view` (4× useful, score=3.963996685)
- `project_panel` (2× useful, score=1.997226655)
- `remote_connection` (2× useful, score=1.970349883)

**Known dead ends** — questions that led nowhere; don't re-derive.
- "Deeply investigate why renaming the worktree is creating a phantom project. We should reproduce it with tests and fix" -> `worktree`, `project`, `workspace`, `sidebar`
- "why sidebar still shows the phantom project and then removes it? maybe the issue is somewhere in the persistent workflow manager?" -> `sidebar`, `worktree`, `project`, `workspace`

## Suggested Questions
_Questions this graph is uniquely positioned to answer:_

- **What is the exact relationship between `Circular Opposing Arrows` and `Refresh or Synchronization`?**
  _Edge tagged AMBIGUOUS (relation: conceptually_related_to) - confidence is low._
- **Why does `gpui` connect `GPUI Project Infrastructure` to `Core Zed Application`?**
  _High betweenness centrality (0.015) - this node is a cross-community bridge._
- **Why does `zed` connect `GPUI Project Infrastructure` to `Core Zed Application`, `Sandbox HTTP Proxy`?**
  _High betweenness centrality (0.010) - this node is a cross-community bridge._
- **Why does `util` connect `GPUI Project Infrastructure` to `Core Zed Application`?**
  _High betweenness centrality (0.005) - this node is a cross-community bridge._
- **What connects `solid`, `color0`, `color1` to the rest of the system?**
  _150 weakly-connected nodes found - possible documentation gaps or missing edges._
- **Should `Core Zed Application` be split into smaller, more focused modules?**
  _Cohesion score 0.14527162977867203 - nodes in this community are weakly interconnected._
- **Should `GPUI Project Infrastructure` be split into smaller, more focused modules?**
  _Cohesion score 0.11807679238871899 - nodes in this community are weakly interconnected._