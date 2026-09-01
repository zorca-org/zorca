# Graph Report - .  (2026-09-01)

## Corpus Check
- 99 files · ~75,087 words
- Verdict: corpus is large enough that graph structure adds value.

## Summary
- 880 nodes · 2390 edges · 135 communities (82 shown, 53 thin omitted)
- Extraction: 96% EXTRACTED · 4% INFERRED · 0% AMBIGUOUS · INFERRED: 90 edges (avg confidence: 0.89)
- Token cost: 0 input · 0 output

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
- Windows sccache Setup
- macOS Icon Verification
- Docker Build Pipeline
- License Generation
- Performance Histogram Tools
- Installation Script
- Linux Packaging Script
- WSL Sandbox Tests
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
- Build Task Runner
- Bindings Header
- Windows Signing Script
- Windows Target Cleanup
- Windows Clippy Runner
- Release Channel Detection
- Dev Drive Capacity
- Windows License Generation
- Windows Version Lookup
- Rustup Installation
- Development Driver Setup
- Nightly Upload
- Dragon SVG Example
- Release Automation
- Build Speed Analysis
- Bind Mount Security
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
- settings_content
- Database Query Workflow
- refineable
- Logging Configuration
- Documentation Assets
- rebuild-client
- Terms RTF Generation
- Crate Version Lookup
- CMake Installation
- MITM Proxy Setup
- New Crate Creation
- Prettier Formatting
- Client Rebuild Script
- Remote Server Management
- Shell Script Linting
- Graphify Update Script
- JSON Schema Updates
- Single Lint Runner
- Auto Update Helper
- Explorer Command Injector
- GPUI Linux Platform
- Media Crate
- Windows Resources
- Xtask Tooling
- Git Commit Guidelines
- Buf Lint Configuration
- Disconnected Sidebar Status
- Remote Restore Environment

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
- `Nightly Builds` --conceptually_related_to--> `Rolling Nightly Release`  [INFERRED]
  README.md → .github/workflows/zorca-ci.yml
- `Selective Hard-Fork Contribution Model` --semantically_similar_to--> `Independent Zed Hard Fork`  [INFERRED] [semantically similar]
  CONTRIBUTING.md → README.md
- `Project and Session Restoration` --semantically_similar_to--> `Remote Codex Resume Investigation`  [INFERRED] [semantically similar]
  README.md → graphify-out/memory/query_20260810_170255_why_can_zorca_not_resume_a_remote_codex_session_af.md
- `GitHub Release Publication` --semantically_similar_to--> `Rolling Nightly Release`  [INFERRED] [semantically similar]
  .github/workflows/release.yml → .github/workflows/zorca-ci.yml
- `CLI Testing Workflow` --conceptually_related_to--> `ZOrca`  [INFERRED]
  crates/cli/README.md → README.md

## Import Cycles
- None detected.

## Hyperedges (group relationships)
- **STE Quality System** — _agents_skills_simple_english_skill_simple_english_skill, _agents_skills_simple_english_references_checklist_verification_checklist, _agents_skills_simple_english_references_use_cases_use_cases_beyond_documentation, _agents_skills_simple_english_references_word_swaps_slop_to_simple_substitutions [EXTRACTED 1.00]
- **Shared Agent Repository Rules** — agents_rust_gpui_repository_rules, claude_rust_gpui_repository_rules, gemini_rust_gpui_repository_rules [EXTRACTED 1.00]
- **ADE Incompatible-Daemon Recovery History** — graphify_out_memory_query_20260810_170255_why_can_zorca_not_resume_a_remote_codex_session_af_remote_codex_resume_investigation, graphify_out_memory_query_20260810_173513_because_i_m_unable_to_connect_to_the_previous_open_legacy_session_reconnect_query, graphify_out_memory_query_20260810_181706_why_does_reconnecting_to_an_older_remote_persisten_incompatible_daemon_handling_query, docs_ade_protocol_compatibility_one_time_compatibility_cut [INFERRED 0.85]
- **ADE LayoutSync Race Boundaries** — graphify_out_memory_query_20260811_145548_why_does_an_uncommitted_changes_file_diff_tab_clos_core_mechanism, graphify_out_memory_query_20260811_165830_another_issue_with_the_render__when_i_click_on_any_core_mechanism, graphify_out_memory_query_20260811_180846_why_does_switching_codex_tabs_or_files_erase_visib_core_mechanism, graphify_out_memory_query_20260811_185406_why_can_closing_mixed_terminal_and_file_tabs_reope_core_mechanism, graphify_out_memory_query_20260811_192542_please_review_the_persistence_server_and_create_te_core_mechanism, graphify_out_memory_query_20260811_201728_please_review_the_persistence_server_and_create_te_core_mechanism [INFERRED 0.95]
- **Remote SSH Transport Resilience** — graphify_out_memory_query_20260811_164207_why_does_an_incompatible_remote_session_daemon_pro_core_mechanism, graphify_out_memory_query_20260811_203801_investigate_why_remote_development_server_is_tryin_core_mechanism, graphify_out_memory_query_20260813_121637_trace_remote_workspace_loading_open_file_path__ssh_core_mechanism, graphify_out_memory_query_20260813_132905_please_investigate_why_after_some_time_working_in_core_mechanism [INFERRED 0.85]
- **Linked Worktree Project Identity** — graphify_out_memory_query_20260811_180846_why_does_one_remote_checkout_appear_twice_as_main_core_mechanism, graphify_out_memory_query_20260811_225318_let_s_do_tests_the_same_way_for_the_sidebar_and_tr_core_mechanism, graphify_out_memory_query_20260812_010538_audit_persisted_ade_workspace_and_project_provenan_core_mechanism, graphify_out_memory_query_20260812_021720_why_does_a_linked_worktree_appear_as_its_own_top_l_core_mechanism [INFERRED 0.95]
- **SSH server creation to folder-picker flow** — graphify_out_memory_query_20260813_154508_ponytail_audit_current_one_file_338_line_diff_for_connection_safe_create_remote_project_reuse, graphify_out_memory_query_20260813_155041_review_only__can_we_dramatically_simplify_current_existing_remote_client_modal_handoff, graphify_out_memory_query_20260813_155227_reduced_patch_is_now_applied__final_read_only_revi_ready_task_remote_client_ownership, graphify_out_memory_query_20260813_155424_after_adding_a_new_ssh_server__open_folder_selecti_authenticated_client_folder_picker_flow [INFERRED 0.95]
- **ADE terminal ownership and workspace recovery** — graphify_out_memory_query_20260814_101802_trace_ade_layout_and_session_ownership_on_first_op_competing_default_terminal_owners, graphify_out_memory_query_20260814_102210_inspect_existing_sidebar_ade_workspaces_gpui_tests_terminal_if_centre_empty_regression_seam, graphify_out_memory_query_20260814_102823_when_a_remote_worktree_is_opened_for_the_first_tim_claimed_ssh_workspace_default_terminal_guard, graphify_out_memory_query_20260814_110107_we_need_to_have_something_that_will_allow_to_drop_workspace_scoped_recovery_over_host_restart, graphify_out_memory_query_20260814_120054_add_a_safe_ui_control_to_drop_stale_daemon_session_retained_workspace_session_reset, graphify_out_memory_query_20260814_144251_i_clicked__kill_and_recreate_session__and_it_creat_pre_reset_terminal_cleanup_and_pending_state [INFERRED 0.85]
- **Workspace identity, host scope, and UI grouping** — graphify_out_memory_query_20260814_090212_root_cause__viral_studio_changed_from_100_78_83_67_same_process_current_session_restore, graphify_out_memory_query_20260814_100022_trace_the_sidebar_project_group_header_renderer_an_workspace_manager_project_row_renderer, graphify_out_memory_query_20260814_100629_how_can_i_understand_what_ssh_url_project_attached_remote_identity_and_checkout_path_display, graphify_out_memory_query_20260814_124358_i_see_workspace_toggle_menu_has_a_lot_of_different_durable_workspace_rows_and_live_session_reconciliation, graphify_out_memory_query_20260814_150017_why_does_the_workspaces_view_show_sessions_that_do_host_scoped_storage_vs_project_id_grouping, graphify_out_memory_query_20260814_150146_please_align_font_on_the_workspaces_view_as_everyw_host_project_grouping_and_inline_spinner, graphify_out_memory_query_20260814_152940_why_are_ade_workspaces_rows_not_scoped_like_the_ma_group_rows_host_project_scope [INFERRED 0.85]
- **ADE Terminal Restoration Integrity** — graphify_out_memory_query_20260814_202218_why_does_an_existing_ade_codex_terminal_show_a_sta_speculative_attach_viewport_corruption, graphify_out_memory_query_20260814_202817_design_the_smallest_deterministic_gpui_regression_staging_pane_attach_regression, graphify_out_memory_query_20260814_210002_when_i_open_a_new_file_from_the_file_explorer__the_commit_gated_terminal_attachment, graphify_out_memory_query_20260819_073415_when_i_connect_to_the_disconnected_session__it_sti_initial_unfocused_viewport_acceptance, graphify_out_memory_query_20260819_194303_the_problem_still_persists__can_you_deeply_investi_truncated_ring_tail_replay [INFERRED 0.95]
- **ADE Project and Worktree Identity** — graphify_out_memory_query_20260814_154730_trace_every_new_terminal_entry_path_relevant_to_an_ade_terminal_cwd_precedence, graphify_out_memory_query_20260814_171829_why_do_new_ade_terminals_start_outside_the_worktre_live_worktree_terminal_and_primary_branch_picker, graphify_out_memory_query_20260815_075140_i_created_a_new_worktree_but_the_sidebar_still_hig_active_workspace_selection_reset, graphify_out_memory_query_20260815_082651_i_clicked_remove_project_on_viral_studio__but_it_r_merged_project_atomic_removal, graphify_out_memory_query_20260823_131436_why_are_workspaces_sessions_not_all_grouped_under_missing_canonical_project_provenance, graphify_out_memory_query_20260823_140714_why_ade_workspace_sessions_from_linked_git_worktre_durable_project_identity_scope, graphify_out_memory_query_20260823_152632_fix_ade_workspaces_project_scoping_and_add_a_kill_canonical_scope_and_kill_all [INFERRED 0.85]
- **Public Website Product Consistency** — website_design_zorca_website_design, website_design_public_content_integrity, website_product_zorca_product, website_product_current_vs_planned_capabilities, website_index_zorca_marketing_page [INFERRED 0.95]
- **Cross-Platform Build and Release Automation** — _github_workflows_release_cross_platform_packaging [INFERRED 0.95]
- **Agent Skill Lifecycle** — crates_agent_skills_readme_agent_skills_design, crates_agent_skills_readme_progressive_disclosure, crates_agent_skills_readme_activation_and_security, crates_agent_skills_builtin_create_skill_skill_creating_a_zed_agent_skill [INFERRED 0.95]
- **GPUI Application Model** — crates_gpui_readme_application, crates_gpui_readme_entity_state_management, crates_gpui_readme_views, crates_gpui_readme_elements, crates_gpui_docs_contexts_gpui_contexts, crates_gpui_docs_key_dispatch_key_dispatch [EXTRACTED 1.00]
- **Linux Sandbox Defense Chain** — crates_sandbox_readme_linux_bind_mount_toctou, crates_sandbox_readme_file_descriptor_validation, crates_sandbox_readme_seccomp_ipc_protection, crates_sandbox_readme_network_proxy_architecture, crates_sandbox_readme_wsl_sandbox_helper [EXTRACTED 1.00]
- **Three-by-Three Circle Arrangement** — crates_gpui_examples_image_color_dark_center_circle, crates_gpui_examples_image_color_green_top_left_circle, crates_gpui_examples_image_color_red_top_center_circle, crates_gpui_examples_image_color_amber_top_right_circle, crates_gpui_examples_image_color_cyan_middle_right_circle, crates_gpui_examples_image_color_translucent_blue_middle_left_circle, crates_gpui_examples_image_color_purple_bottom_center_circle, crates_gpui_examples_image_color_magenta_bottom_right_circle, crates_gpui_examples_image_color_translucent_rose_bottom_left_circle [INFERRED 0.95]
- **Dragon Anatomical Composition** — crates_gpui_examples_svg_dragon_dragon_head, crates_gpui_examples_svg_dragon_coiled_dragon_body, crates_gpui_examples_svg_dragon_horns_and_antlers, crates_gpui_examples_svg_dragon_flame_like_mane, crates_gpui_examples_svg_dragon_fangs_and_tusks [INFERRED 0.95]

## Communities (135 total, 53 thin omitted)

### Community 0 - "Core Zed Application"
Cohesion: 0.10
Nodes (173): activity_indicator, agent_settings, agent_skills, agent_workspaces, anthropic, askpass, assets, audio (+165 more)

### Community 1 - "GPUI Project Infrastructure"
Cohesion: 0.07
Nodes (84): AtlasTile, Background, Bounds_ScaledPixels, constant, Corners_ScaledPixels, blur_along_x(), corner_dash_velocity(), dash_alpha() (+76 more)

### Community 2 - "Metal Rendering Shaders"
Cohesion: 0.08
Nodes (22): BuildRemoteServer(), BuildZorcaAndItsFriends(), aliasBlockRegex(), CARGO_ALIASES, detectShell(), expandAlias(), findLatestTimingFile(), findSubcommand() (+14 more)

### Community 3 - "Build and Packaging Tools"
Cohesion: 0.07
Nodes (28): App Context, AsyncApp and AsyncWindowContext, Context<T>, Entity<T>, GPUI Contexts, TestAppContext, Window, Keyboard Action (+20 more)

### Community 4 - "Renovate Dependency Configuration"
Cohesion: 0.12
Nodes (21): Website Accessibility and Reduced-Motion Requirements, Cobalt, Violet, and Coral Visual Identity, Authentic Product Capture and Claim Integrity, Responsive Editorial Product Layout, ZOrca Website Design, Agent Workspace Hero, Current Capability Stories, Installation Section (+13 more)

### Community 5 - "Prettier Server Protocol"
Cohesion: 0.12
Nodes (16): after 3pm on Wednesday, config:recommended, :dependencyDashboardApproval, group:serdeMonorepo, helpers:pinGitHubActionDigests, **/node_modules/**, :semanticCommitsDisabled, :separateMultipleMajorReleases (+8 more)

### Community 6 - "macOS Bundle Pipeline"
Cohesion: 0.24
Nodes (11): { Buffer }, fs, handleBuffer(), handleMessage(), makeError(), { once }, path, Prettier (+3 more)

### Community 7 - "Cargo Timing Analysis"
Cohesion: 0.14
Nodes (14): LayoutSync Self-Echo Replaces ProjectDiff, Why does an Uncommitted Changes file diff tab close after a delay in an ADE workspace?, LayoutSync Identical-Layout Revision Guard, Why do footer and terminal-tab clicks fully rerender visible tab content?, LayoutSync Selection-Only Structural Comparison, Why does switching Codex tabs or files erase visible terminal history?, ADE Layout Ownership Without LayoutSync, What causes the ADE terminal layout ownership error? (+6 more)

### Community 8 - "Linux Bundle Pipeline"
Cohesion: 0.21
Nodes (12): bundle-mac script, CARGO_BUNDLE_SKIP_BUILD, CXXFLAGS, download_and_unpack(), download_git(), help_info(), RELEASE_CHANNEL, restore_bundle_manifest() (+4 more)

### Community 9 - "Metal Debugging Tools"
Cohesion: 0.19
Nodes (13): STE Verification Checklist, Technical Communication Adaptations, STE Use Cases Beyond Documentation, Plain-Language Replacement, Slop-to-Simple Substitutions, ASD-STE100 Issue 9, Descriptive Writing, Pragmatic STE Mode (+5 more)

### Community 10 - "Terminal Color Diagnostics"
Cohesion: 0.17
Nodes (13): Rust and GPUI Repository Rules, Rust and GPUI Repository Rules, ZOrca Contributing Guide, Pull Request Hygiene, Selective Hard-Fork Contribution Model, CLI Testing Workflow, Rust and GPUI Repository Rules, Agent Development Environment (+5 more)

### Community 11 - "Highlight Analysis"
Cohesion: 0.26
Nodes (11): analyzeTimings(), args, extractUnitData(), formatTime(), formatUnit(), fs, getZedDataDir(), os (+3 more)

### Community 12 - "License Compliance Checks"
Cohesion: 0.20
Nodes (9): bundle-linux script, APP_ARGS, APP_CLI, APP_ICON, CC, DO_STARTUP_NOTIFY, help_info(), RELEASE_VERSION (+1 more)

### Community 13 - "sccache Setup"
Cohesion: 0.18
Nodes (10): metal-debug script, DYLD_INSERT_LIBRARIES, DYLD_LIBRARY_PATH, DYMTL_TOOLS_DYLIB_PATH, GPUProfilerEnabled, GPUTOOLS_LOAD_GTMTLCAPTURE, LD_LIBRARY_PATH, METAL_DEBUG_ERROR_MODE (+2 more)

### Community 14 - "Uninstallation Scripts"
Cohesion: 0.24
Nodes (10): Creating a Zed Agent Skill, Skill File Format, Skill Scope Selection, Skill Activation and Security, Agent Skills Design, Agent Skills Specification, Flat Two-Scope Skill Discovery, Skill Progressive Disclosure (+2 more)

### Community 15 - "Flatpak Bundling"
Cohesion: 0.20
Nodes (10): Development Extension Installation, Extension API Compatibility, Extension Manifest, Extension Trait, WebAssembly Extension Packaging, Zed Rust Extension API, Extension Test Fixture, Zed Extension Ecosystem Compatibility (+2 more)

### Community 16 - "Linux Installation"
Cohesion: 0.20
Nodes (10): Amber Top-Right Circle, Cyan Middle-Right Circle, Dark Center Circle, Green Top-Left Circle, Magenta Bottom-Right Circle, Multicolor Grip Circle Grid, Purple Bottom-Center Circle, Red Top-Center Circle (+2 more)

### Community 17 - "Blob Store Uploads"
Cohesion: 0.20
Nodes (10): CC BY-SA 3.0 License, Coiled Dragon Body, Dragon Clip Art, Dragon Head, Fangs and Tusks, Flame-Like Mane, Flat Vector Illustration, Horns and Antlers (+2 more)

### Community 18 - "Windows sccache Setup"
Cohesion: 0.20
Nodes (10): Concurrent Daemon Upgrade Shared Upload Collision, Why does an incompatible remote session daemon prompt several times and fail to upgrade with Text file busy?, ADE Terminal Focus and PTY Resize Suppression, Why does an ADE-backed Codex terminal visibly repaint when footer panel buttons are clicked?, Dirty Remote-Server Source Identity Rebuild Race, Why is the remote development server uploaded again on the same host?, SSH ControlMaster Validation and Single-Owner Forwarding, Trace remote loading, SSH ControlPath, forwarding, and reconnect failures (+2 more)

### Community 19 - "macOS Icon Verification"
Cohesion: 0.20
Nodes (10): viral-studio SSH identity restoration investigation, Same-process reopen must restore the current process session, Durable workspace rows are distinct from live session inventory, Workspace toggle row reconciliation investigation, Host-scoped storage is obscured by project_id-only grouping, Workspaces view project and host scoping investigation, Host-project grouping with normal labels and inline SpinnerLabel, Workspaces view typography, loader, and grouping request (+2 more)

### Community 20 - "Docker Build Pipeline"
Cohesion: 0.22
Nodes (9): Forced Replacement Requires Consent, One-Time Compatibility Cut, Pre-Cut Daemon Recovery Proposal, Remote Codex Resume Investigation, Bounded Generation-One Reconnect, Legacy Session Reconnect Query, Generation-Two-Only Forced Upgrade, Incompatible Daemon Handling Query (+1 more)

### Community 21 - "License Generation"
Cohesion: 0.32
Nodes (8): Artifact Cleanup Workflow, Newest Artifact Retention, Deploy Website Workflow, GitHub Pages Deployment, Linux Package Build, Rolling Nightly Release, Windows Package Build, ZOrca CI Workflow

### Community 22 - "Performance Histogram Tools"
Cohesion: 0.25
Nodes (8): Hostile-Code Security Model, macOS Seatbelt Policy, Sandbox Network Proxy Architecture, Platform-Specific Sandbox Implementations, Sandbox, SandboxPolicy, SandboxFilesystemLocation, Seccomp IPC-Socket Protection

### Community 23 - "Installation Script"
Cohesion: 0.29
Nodes (8): Additive Protocol Evolution, Capability Negotiation, ADE Session-Daemon Compatibility Contract, Degraded Persistence Mode, FIFO Persist Worker, Generation Handshake, Mutation Persistence Classes, Persist-Before-Ack Contract

### Community 24 - "Linux Packaging Script"
Cohesion: 0.25
Nodes (8): build_tree ProjectGroupKey Row Coalescing, Why does one remote checkout appear twice as main in the project sidebar?, Git Metadata Canonicalization for Worktree Identity, Stress-test sidebar worktree and project grouping, Persisted Workspace Identity Migration, Audit persisted ADE workspace and project provenance for linked worktrees, Linked Worktree Sidebar Restore Hardening, Why does a linked worktree become a top-level sidebar project after restart?

### Community 25 - "WSL Sandbox Tests"
Cohesion: 0.25
Nodes (8): New SSH Server Folder-Picker Transition, Trace why a new SSH server returns to the aggregate projects picker, New SSH Server ProjectPicker Regression Design, Design the smallest regression test for new SSH server folder selection, Folder-Picker Lifecycle, Focus, and Index Review, Review the new-server folder-picker fix and test, Cancellable Focused Folder-Picker Handoff Fix, Review the remote_servers.rs fix for correctness and test validity

### Community 26 - "Color SVG Example"
Cohesion: 0.25
Nodes (8): Minimal new-server to project-picker audit, Connection-safe create_remote_project reuse, Existing RemoteClient survives modal replacement through a ready task, Existing remote-project flow simplification review, Ready task owns RemoteClient across modal replacement, Reduced exact-session patch review, Authenticated client enters ProjectPicker without reconnecting, Open folder selection after adding an SSH server

### Community 27 - "Sandbox HTTP Proxy"
Cohesion: 0.25
Nodes (8): Daemon session recovery control design, Workspace-scoped recovery is safer than host daemon restart, Kill Persistent Workspace is the routine recovery control, Persistent workspace recovery control recommendation, Safe worktree session reset implementation, Retain editor layout while replacing exact-worktree sessions, Blank terminal after session reset investigation, Close old terminals and show pending state before a slow reset

### Community 28 - "Extension Test API"
Cohesion: 0.29
Nodes (7): Build Identity Invalidation, Build Optimization Order, Application Build Speed Analysis, Release Packaging Bottleneck, Redundant Release Debuginfo, Repeated License Generation, Serial Application and Remote-Server Builds

### Community 29 - "Bind Source TOCTOU Test"
Cohesion: 0.60
Nodes (4): print_blocks(), print_colour(), print_run(), print256color.sh script

### Community 30 - "True Color Demo"
Cohesion: 0.33
Nodes (6): Folder-picker task ownership and SSH server index race, Folder-picker handoff final review, Concurrent SSH additions can reserve the same server index, Remote settings race reassessment, Deferred RemoteClient lifetime is safe but index allocation can race, Reduced remote-client patch validation

### Community 31 - "Zed Launcher Script"
Cohesion: 0.33
Nodes (6): Sidebar and ADE compete to create the first terminal, ADE first-open layout and session ownership trace, TerminalIfCentreEmpty must defer to an SSH workspace claim, Duplicate-terminal GPUI regression design, Claimed SSH workspace suppresses the stock default terminal, First remote-worktree duplicate terminal investigation

### Community 32 - "Icon Generation"
Cohesion: 0.33
Nodes (6): Speculative ADE Attach Corrupts Shared PTY Viewport, Staging Pane Attach Regression Boundary, Commit-Gated ADE Terminal Attachment, Initial Unfocused ADE Viewport Acceptance, OutputHub Truncated Ring Tail Replay, LayoutSync Persistence for Every ItemRemoved Event

### Community 33 - "Keymap Validation"
Cohesion: 0.60
Nodes (5): count_instances(), find_highlight_files(), main(), parse_arguments(), print_instances()

### Community 34 - "TODO Validation"
Cohesion: 0.60
Nodes (5): check-licenses script, check_license(), check_manifest_for_agpl(), check_no_agpl_license_file(), check_symlink_target()

### Community 35 - "Target Directory Cleanup"
Cohesion: 0.33
Nodes (6): Cargo Fingerprint Clean, Current Custom Lints, Dylint Library, --force-warn Requirement, Pinned Nightly Toolchain, single-lint Helper

### Community 36 - "Clippy Runner"
Cohesion: 0.40
Nodes (5): Icon Contribution Flow, Icon Design Guidelines, Lucide, Phosphor, Zed Icons

### Community 37 - "Crate Dependency Graph"
Cohesion: 0.40
Nodes (5): Protocol Envelope, Stable Error Frames, Fixed Wire Constraints, Request-Scoped Failure Boundary, Two-Stage Frame Decoding

### Community 38 - "CLI Debugging"
Cohesion: 0.40
Nodes (5): ZOrca Brand Palette, Distinct Product Identity, Application Icon Generation, ZOrca Logo Source of Truth, ZOrca Branding

### Community 39 - "WASI SDK Download"
Cohesion: 0.40
Nodes (5): Phantom Project Rename Investigation, Stale Folder-Key Cleanup, Stable Repository-Key Fix, Transient Phantom Project Investigation, Worktree-Isolated Agent Workspace

### Community 40 - "Dependency Installer"
Cohesion: 0.40
Nodes (5): ADE Terminal CWD Precedence: Explicit Path, Live Project Root, Persisted Fallback, ADE Terminal and Worktree Picker Regression Seams, Worktree Picker Regression Reliability, Independent Default-Branch Resolution and Remote-Name Classification, Live Worktree Terminal CWD and Primary-Branch Worktree Picker

### Community 41 - "License CSV Generation"
Cohesion: 0.70
Nodes (4): setup-sccache script, configure_sccache(), install_sccache(), show_config()

### Community 42 - "Terms RTF Generation"
Cohesion: 0.80
Nodes (4): linux(), macos(), prompt_remove_preferences(), uninstall.sh script

### Community 43 - "Crate Version Lookup"
Cohesion: 0.67
Nodes (4): Deterministic GPUI Test Scheduler, GPUI Executor Timer Preference, GPUI Test Debugging, Parking Failure Diagnostics

### Community 44 - "CMake Installation"
Cohesion: 0.50
Nodes (4): Dynamic Channel Branch Mapping, Prerequisite-Aware Conflict Resolution, Canonical Cherry-Pick Script, Zed Cherry-Pick Procedure

### Community 45 - "MITM Proxy Script"
Cohesion: 0.83
Nodes (4): Linux Pull Request Validation, Pull Request Checks Workflow, Windows Pull Request Validation, Workspace Validation Suite

### Community 46 - "New Crate Generator"
Cohesion: 0.50
Nodes (4): Cross-Platform Packaging, GitHub Release Publication, Release Tag Validation, Release Workflow

### Community 47 - "Prettier Runner"
Cohesion: 0.67
Nodes (4): migrator, settings_content, settings_json, settings_macros

### Community 48 - "Remote Server Script"
Cohesion: 0.50
Nodes (4): File-Descriptor Bind Validation, HostFilesystemLocation, Linux Bind-Mount TOCTOU Attack, WSL Sandbox Helper

### Community 49 - "Shell Script Validation"
Cohesion: 0.50
Nodes (4): Neovim Result Cache, NeovimBackedTestContext, VimTestContext, Zed Vim Mode

### Community 50 - "JSON Schema Update"
Cohesion: 0.50
Nodes (4): Managed Center-Terminal Fix, Terminal File-Click Investigation, Layout Revision Race Fix, Scrollback Loss Investigation

### Community 51 - "Single Lint Runner"
Cohesion: 0.50
Nodes (4): OpenTerminal Center-Terminal Routing, Why does Open in Terminal not open anything?, OpenTerminal ADE CWD Fix and GPUI Regression, Implement the Open in Terminal fix and regression test

### Community 52 - "Auto Update Helper"
Cohesion: 0.50
Nodes (4): Sidebar project-group renderer trace, workspace_manager::render_row is the shared project-row renderer, Remote identity and checkout path with full tooltip, Remote project identity display request

### Community 53 - "Explorer Command Injection"
Cohesion: 0.50
Nodes (3): bundle-flatpak script, ARCHIVE, CHANNEL

### Community 54 - "GPUI Linux Backend"
Cohesion: 0.50
Nodes (3): install-linux script, ZORCA_BUNDLE_PATH, ZORCA_CHANNEL

### Community 55 - "Media Module"
Cohesion: 0.83
Nodes (3): UploadToBlobStore(), UploadToBlobStorePublic(), UploadToBlobStoreWithACL()

### Community 57 - "Build Task Runner"
Cohesion: 0.83
Nodes (3): verify-macos-document-icon script, fail(), usage()

### Community 58 - "Bindings Header"
Cohesion: 0.50
Nodes (4): Diagonal Navy-to-Coral Brand Color Field, White Fused ZO Monogram, Rounded-Square Avatar Tile, ZOrca GitHub Organization Avatar

### Community 59 - "Windows Signing Script"
Cohesion: 0.67
Nodes (3): Commit Message Structure, Conventional Commit Workflow, Conventional Commits Specification

### Community 60 - "Windows Target Cleanup"
Cohesion: 0.67
Nodes (3): Clippy Redundancy Check, Dylint Creation Rules, Lint UI Test Requirement

### Community 61 - "Windows Clippy Runner"
Cohesion: 0.67
Nodes (3): ZOrca Bug Report, Upstream Zed Issue Routing, Issue Template Configuration

### Community 62 - "Release Channel Detection"
Cohesion: 0.67
Nodes (3): Pull Request Template, Release Notes Format, Application Startup Validation

### Community 63 - "Dev Drive Capacity"
Cohesion: 0.67
Nodes (3): env_var, gpui_shared_string, zed_env_vars

### Community 64 - "Windows License Generation"
Cohesion: 0.67
Nodes (3): Conversation Compaction Handoff, Detailed Conversation Summary, Conversation Title Generation

### Community 65 - "Windows Version Lookup"
Cohesion: 0.67
Nodes (3): Batched Breaking Release, Extension API Breaking Changes, SlashCommand.menu_text Rename

### Community 66 - "Rustup Installation"
Cohesion: 0.67
Nodes (3): Circular Opposing Arrows Icon, Circular Opposing Arrows, Refresh or Synchronization

### Community 67 - "Development Driver Setup"
Cohesion: 0.67
Nodes (3): Project Assets, Zed Feature Documentation, ZOrca Product Documentation

### Community 68 - "Nightly Upload"
Cohesion: 0.67
Nodes (3): Missing Canonical ADE Project Provenance, Durable ProjectGroupKey-Derived ADE Project Identity, Canonical ADE Project Scope and Kill All Sessions

## Ambiguous Edges - Review These
- `Circular Opposing Arrows` → `Refresh or Synchronization`  [AMBIGUOUS]
  crates/gpui/examples/image/arrow_circle.svg · relation: conceptually_related_to

## Knowledge Gaps
- **224 isolated node(s):** `solid`, `color0`, `color1`, `tile_position`, `clip_distance` (+219 more)
  These have ≤1 connection - possible missing edges or undocumented components.
- **53 thin communities (<3 nodes) omitted from report** — run `graphify query` to explore isolated nodes.

## Suggested Questions
_Questions this graph is uniquely positioned to answer:_

- **What is the exact relationship between `Circular Opposing Arrows` and `Refresh or Synchronization`?**
  _Edge tagged AMBIGUOUS (relation: conceptually_related_to) - confidence is low._
- **Why does `gpui` connect `Core Zed Application` to `Prettier Runner`, `Dev Drive Capacity`?**
  _High betweenness centrality (0.011) - this node is a cross-community bridge._
- **Why does `zed` connect `Core Zed Application` to `ZOrca Product Website`, `Dev Drive Capacity`, `Prettier Runner`?**
  _High betweenness centrality (0.007) - this node is a cross-community bridge._
- **Why does `util` connect `Core Zed Application` to `Prettier Runner`?**
  _High betweenness centrality (0.003) - this node is a cross-community bridge._
- **What connects `solid`, `color0`, `color1` to the rest of the system?**
  _224 weakly-connected nodes found - possible documentation gaps or missing edges._
- **Should `Core Zed Application` be split into smaller, more focused modules?**
  _Cohesion score 0.10364296276381234 - nodes in this community are weakly interconnected._
- **Should `GPUI Project Infrastructure` be split into smaller, more focused modules?**
  _Cohesion score 0.07086834733893557 - nodes in this community are weakly interconnected._