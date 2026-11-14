# What AtticARC must keep doing

Every rule the code keeps, one line each, with the test that holds it. The refactor
([refactor-brief.md](refactor-brief.md)) may move anything, but every test named here must pass
unchanged at every step. Where a rule and the code disagree, the code as of `1767fdd` (14 Nov
2026) is right and this file is wrong.

Tests are in `crates/atticarc-core`: `archive::`, `cleanup::`, `scan::`, `disk::`, `exclude::`,
`settings::`, `journal::` are unit tests in `src/`; `workflow::` and `fixtures::` are in `tests/`.
`soak::` is in `crates/atticarc-cli`.

## Archiving

| Rule | Test |
|---|---|
| Each folder becomes one `.7z` holding every file and folder (empty ones, Unicode names too), and counts only once it has been decoded and every file matched its hash | `archive::archive_verify_and_catalog`, `workflow::archive_duplicates_excluded_cleanup_rebuild_compare` |
| Every file is hashed (BLAKE3 and SHA-256) as it is compressed; a file that changes while being read fails the folder | `archive::archive_verify_and_catalog` |
| Already-compressed formats are stored without recompressing when store mode is on, and compressed when it is off | `archive::archive_verify_and_catalog` (both modes) |
| The source folder is never changed by archiving | `workflow::archive_duplicates_excluded_cleanup_rebuild_compare` |
| A cancelled or failed job leaves no archive and no partial file | `archive::cancel_leaves_no_partial` |
| A destination without room is refused before anything is written | `workflow::a_folder_that_does_not_fit_is_refused_before_anything_is_written`, `disk::ensure_free_passes_small_needs_and_refuses_impossible_ones` |
| The destination can't be inside the source, nor in a protected folder (refused before it is created) | `archive::refuses_output_inside_source`, `archive::output_inside_protected_folder_is_refused_before_it_is_created` |
| Links and junctions are listed, never followed or archived, and block a delete | `workflow::links_are_listed_not_archived_and_block_a_delete`, `scan::symlinks_are_not_followed` |
| A taken archive name gives `Name (2).7z`, `Name (3).7z`…, each with its own record | `archive::same_name_different_content_gets_a_numbered_archive`, `archive::second_archive_gets_a_new_name` |
| Archive paths are stored without the Windows `\\?\` prefix; the source keeps its spelling | `archive::archive_path_is_stored_in_plain_form`, `archive::original_folder_keeps_its_spelling` |
| A `.atticarc` folder left by extracting an archive is not archived again | `archive::extracted_manifest_is_not_archived_again` |
| Archiving a folder again makes a new archive; only once it is verified, the older one is moved to `.atticarc\superseded` and drops out of every list; nothing is deleted | `archive::archiving_again_applies_new_exclusions_and_moves_the_old_archive_aside` |
| With a detailed log, every phase and file is named; without one, nothing is kept | `archive::a_detailed_log_names_every_file` |

## Exclusions

| Rule | Test |
|---|---|
| Wildcards (`*` within a folder, `**` across, `?`) and regexes match the path inside the folder, ignoring case; a rule that doesn't compile stops the job | `exclude::wildcards`, `exclude::regexes_and_switches` |
| Set aside wins over discard when both match | `exclude::set_aside_wins_over_discard` |
| Set-aside files go in an archive of their own inside the main one; discarded files are archived nowhere but listed with their hashes | `archive::excluded_files_are_set_aside_or_discarded`, `workflow::…` |
| Left-out files can be put back into the archive, or the rest discarded | `archive::excluded_files_go_back_into_the_archive` |
| Set-aside archives beside older archives still work, and can be moved inside | `archive::set_aside_archives_beside_older_archives_still_work_and_move_inside` |

## Duplicates

| Rule | Test |
|---|---|
| Duplicates are files with the same size and BLAKE3 hash, found across all archives | `archive::search_duplicates_and_extract`, `workflow::…` |
| A duplicate is never removed if it would be the last copy, nor if reference files point to it | `archive::duplicates_are_removed_but_never_the_last_copy`, `archive::duplicates_are_replaced_by_references_and_put_back` |
| Replacing checks every original by decoding it first; a dry run or a missing original changes nothing; a replaced file can be extracted from its original or put back | `archive::duplicates_are_replaced_by_references_and_put_back` |
| Stored once: identical files of 64 KiB or more in one folder are compressed once, later copies become `.atticarcdep` references to the first; smaller ones and same-size files that differ are kept as they are; the folder still passes its check | `archive::identical_files_in_a_folder_are_stored_once` |
| The fast scan finds possible duplicates by size and confirms them by hash; a file changed since the scan is an error, not a duplicate | `scan::*` |

## Clean up

| Rule | Test |
|---|---|
| A folder is deleted only if its saved check passed | `cleanup::an_unchecked_folder_is_never_deleted`, `workflow::…` |
| The full check: every file on disk is in the archive's record with the same size and hash, and every file in the archive decodes to its hash | `archive::check_passes_then_delete`, `archive::check_fails_when_a_file_changed`, `archive::check_fails_when_a_file_was_added`, `archive::check_fails_when_the_archive_is_damaged` |
| On a drive with a change journal, the check is saved when the folder is archived, and the folder can be deleted in one step | `cleanup::a_folder_is_checked_when_archived_and_deleted_in_one_step` (NTFS only) |
| Before a delete: the folder's snapshot is unchanged since its check | `cleanup::a_folder_changed_after_its_check_is_left_alone`, `archive::a_folder_snapshot_changes_when_the_folder_does`, `workflow::…` |
| Before a delete: the archive's size, modified time and header are unchanged | `cleanup::a_changed_archive_is_left_alone`, `archive::index_and_scrub_catch_what_they_should` |
| An archive last decoded more than 30 days ago is decoded again before its folder is deleted | `cleanup::an_archive_not_decoded_lately_is_decoded_before_deleting` |
| Scrub lists archives oldest-decoded first and decodes them, reading only the archives | `cleanup::scrub_lists_and_decodes` |
| A protected folder is never checked or deleted, and shows as protected, not failed | `cleanup::a_protected_folder_is_shown_as_protected_not_failed`, `archive::protected_paths` |
| Nothing is ticked to start; checks and ticks survive a restart; a deleted folder's tick is dropped | `cleanup::a_check_is_saved_and_survives_a_restart`, `cleanup::ticks_are_saved_and_dropped_once_deleted`, `workflow::…` |
| Deleting removes the folder (read-only files too), keeps the archive and marks it in the catalogue | `archive::check_passes_then_delete`, `workflow::…` |

## Catalogue

| Rule | Test |
|---|---|
| Reading a destination with no catalogue creates nothing | `archive::reading_a_missing_catalogue_creates_nothing`, `workflow::…` |
| The catalogue can be rebuilt from the archives' manifests, and rebuilding twice adds nothing | `archive::rebuilding_the_catalogue_from_the_archives`, `workflow::…`, `fixtures::the_fixture_archives_still_verify_check_scrub_and_rebuild` |
| Clearing the catalogue keeps the archives | `archive::clearing_the_catalogue_keeps_the_archives` |
| An archive is known by what it holds, not its name: a renamed archive is relinked (its old name still resolves), never added twice; a missing one with no match stays missing | `archive::a_renamed_archive_is_relinked_not_added_twice`, `archive::a_missing_archive_with_no_match_stays_missing` |
| A renamed source folder is found by its files | `archive::a_renamed_source_folder_is_found_by_its_files` |
| A catalogue left loose by earlier versions moves into `.atticarc` | `archive::old_catalogue_moves_into_its_folder` |
| Search finds files by words of their names | `archive::search_duplicates_and_extract`, `workflow::…` |

## Formats (frozen)

| Rule | Test |
|---|---|
| Archives and the catalogue made on 14 Nov 2026 still open, list, verify, check, scrub, extract and rebuild | `fixtures::the_fixture_catalogue_still_opens`, `fixtures::the_fixture_archives_still_verify_check_scrub_and_rebuild` |
| The same folder archived now gives a manifest with the same fields and the same contents (times and paths aside) | `fixtures::archiving_the_same_folders_now_gives_the_same_manifests` |

## Settings, disk and the rest

| Rule | Test |
|---|---|
| Recent folders: most recent first, no duplicates, at most eight | `settings::remember_keeps_most_recent_first_without_duplicates` |
| Settings saved by foldarc still load; the default destination sits next to the source | `settings::foldarc_settings_still_parse`, `settings::out_dir_sits_next_to_source` |
| The worst case allowed for is the input plus 1%; free space is found for a folder that doesn't exist yet | `disk::*` |
| NTFS change-journal numbers are read from v2 and v3 records | `journal::reads_the_usn_from_v2_and_v3_records` |
| A whole round (archive, check, put back, de-duplicate, rebuild) holds together | `soak::one_round_on_a_small_tree` |

## Rules with no automatic test

These are kept by hand: try them on WinX after any change to the GUI.

- The window never blocks on the disk: every catalogue or disk read runs in the background with a spinner.
- Anything destructive is opt-in, with a confirmation that lists what goes.
- Plain English in the UI: say what will happen and what was done.
- Settings (colour, background, dark mode, detailed log, compatible graphics) are saved as changed and kept between runs.
