RemovalRecordsIntegrity-4in-4out:
| Subroutine                                                                                                                    |            Processor |             Op Stack |                  RAM |                 Hash |                  U32 |
|:------------------------------------------------------------------------------------------------------------------------------|---------------------:|---------------------:|---------------------:|---------------------:|---------------------:|
| tasmlib_structure_verify_nd_si_integrity___RemovalRecordsIntegrityWitnessMemory                                               |       18448 ( 24.0%) |       13058 ( 24.7%) |        1357 (  2.4%) |           0 (  0.0%) |         618 (  2.4%) |
| ··tasmlib_structure_tasmobject_verify_size_indicators_dyn_elem_sizes___RemovalRecord                                          |       17771 ( 23.1%) |       12608 ( 23.8%) |        1320 (  2.4%) |           0 (  0.0%) |         270 (  1.0%) |
| ····tasmlib_structure_tasmobject_verify_size_indicators_dyn_elem_sizes___tuple_L_u64_tuple_L_MmrMembershipProof_Chunk_R_R     |       17465 ( 22.7%) |       12404 ( 23.4%) |        1304 (  2.3%) |           0 (  0.0%) |          90 (  0.3%) |
| ··tasmlib_tasmobject_size_verifier_option_none                                                                                |           4 (  0.0%) |           2 (  0.0%) |           0 (  0.0%) |           0 (  0.0%) |           0 (  0.0%) |
| ··tasmlib_structure_tasmobject_verify_size_indicators_dyn_elem_sizes___Utxo                                                   |         466 (  0.6%) |         308 (  0.6%) |          24 (  0.0%) |           0 (  0.0%) |          90 (  0.3%) |
| ····tasmlib_structure_tasmobject_verify_size_indicators_dyn_elem_sizes___Coin                                                 |         236 (  0.3%) |         160 (  0.3%) |          12 (  0.0%) |           0 (  0.0%) |          30 (  0.1%) |
| tasmlib_mmr_bag_peaks                                                                                                         |         402 (  0.5%) |         700 (  1.3%) |         277 (  0.5%) |         318 (  0.9%) |          16 (  0.1%) |
| ··tasmlib_mmr_bag_peaks_length_is_not_zero                                                                                    |         372 (  0.5%) |         674 (  1.3%) |         275 (  0.5%) |         318 (  0.9%) |           0 (  0.0%) |
| ····tasmlib_mmr_bag_peaks_length_is_not_zero_or_one                                                                           |         356 (  0.5%) |         662 (  1.3%) |         275 (  0.5%) |         318 (  0.9%) |           0 (  0.0%) |
| ······tasmlib_mmr_bag_peaks_loop                                                                                              |         320 (  0.4%) |         636 (  1.2%) |         265 (  0.5%) |         318 (  0.9%) |           0 (  0.0%) |
| tasmlib_hashing_merkle_verify                                                                                                 |         105 (  0.1%) |          84 (  0.2%) |           0 (  0.0%) |          54 (  0.2%) |          58 (  0.2%) |
| ··tasmlib_hashing_merkle_verify_tree_height_is_not_zero                                                                       |          36 (  0.0%) |           6 (  0.0%) |           0 (  0.0%) |          54 (  0.2%) |          33 (  0.1%) |
| ····tasmlib_hashing_merkle_verify_traverse_tree                                                                               |          21 (  0.0%) |           0 (  0.0%) |           0 (  0.0%) |          54 (  0.2%) |          33 (  0.1%) |
| tasmlib_hashing_algebraic_hasher_hash_varlen                                                                                  |       30806 ( 40.1%) |       20589 ( 38.9%) |       50538 ( 90.5%) |       30357 ( 85.5%) |          42 (  0.2%) |
| ··tasmlib_hashing_absorb_multiple                                                                                             |       30764 ( 40.1%) |       20544 ( 38.8%) |       50538 ( 90.5%) |       30336 ( 85.4%) |          42 (  0.2%) |
| ····tasmlib_hashing_absorb_multiple_hash_all_full_chunks                                                                      |       30336 ( 39.5%) |       20224 ( 38.2%) |       50530 ( 90.5%) |       30318 ( 85.4%) |           0 (  0.0%) |
| ····tasmlib_hashing_absorb_multiple_pad_varnum_zeros                                                                          |         227 (  0.3%) |         145 (  0.3%) |           0 (  0.0%) |           0 (  0.0%) |           0 (  0.0%) |
| ····tasmlib_hashing_absorb_multiple_read_remainder                                                                            |          90 (  0.1%) |          52 (  0.1%) |           8 (  0.0%) |           0 (  0.0%) |           0 (  0.0%) |
| for_all_utxos                                                                                                                 |       26570 ( 34.6%) |       18134 ( 34.2%) |        3652 (  6.5%) |        2728 (  7.7%) |       25464 ( 97.2%) |
| ··tasmlib_hashing_algebraic_hasher_hash_varlen                                                                                |         624 (  0.8%) |         468 (  0.9%) |          76 (  0.1%) |          76 (  0.2%) |          11 (  0.0%) |
| ····tasmlib_hashing_absorb_multiple                                                                                           |         568 (  0.7%) |         408 (  0.8%) |          76 (  0.1%) |          48 (  0.1%) |          11 (  0.0%) |
| ······tasmlib_hashing_absorb_multiple_hash_all_full_chunks                                                                    |          48 (  0.1%) |          32 (  0.1%) |          40 (  0.1%) |          24 (  0.1%) |           0 (  0.0%) |
| ······tasmlib_hashing_absorb_multiple_pad_varnum_zeros                                                                        |          24 (  0.0%) |          16 (  0.0%) |           0 (  0.0%) |           0 (  0.0%) |           0 (  0.0%) |
| ······tasmlib_hashing_absorb_multiple_read_remainder                                                                          |         348 (  0.5%) |         196 (  0.4%) |          36 (  0.1%) |           0 (  0.0%) |           0 (  0.0%) |
| ··tasmlib_neptune_mutator_set_commit                                                                                          |          16 (  0.0%) |          40 (  0.1%) |           0 (  0.0%) |          48 (  0.1%) |           0 (  0.0%) |
| ··tasmlib_mmr_verify_from_secret_in_leaf_index_on_stack                                                                       |        7368 (  9.6%) |        3848 (  7.3%) |          20 (  0.0%) |        1440 (  4.1%) |        7245 ( 27.7%) |
| ····tasmlib_mmr_leaf_index_to_mt_index_and_peak_index                                                                         |         496 (  0.6%) |         332 (  0.6%) |           0 (  0.0%) |           0 (  0.0%) |        1306 (  5.0%) |
| ······tasmlib_arithmetic_u64_lt_preserve_args                                                                                 |          52 (  0.1%) |          44 (  0.1%) |           0 (  0.0%) |           0 (  0.0%) |         252 (  1.0%) |
| ······tasmlib_arithmetic_u64_log_2_floor                                                                                      |          52 (  0.1%) |          36 (  0.1%) |           0 (  0.0%) |           0 (  0.0%) |         120 (  0.5%) |
| ········tasmlib_arithmetic_u64_log_2_floor_hi_not_zero                                                                        |          28 (  0.0%) |          20 (  0.0%) |           0 (  0.0%) |           0 (  0.0%) |         120 (  0.5%) |
| ······tasmlib_arithmetic_u64_pow2                                                                                             |          20 (  0.0%) |          12 (  0.0%) |           0 (  0.0%) |           0 (  0.0%) |          37 (  0.1%) |
| ······tasmlib_arithmetic_u64_decr                                                                                             |          80 (  0.1%) |          64 (  0.1%) |           0 (  0.0%) |           0 (  0.0%) |           0 (  0.0%) |
| ········tasmlib_arithmetic_u64_decr_carry                                                                                     |          48 (  0.1%) |          40 (  0.1%) |           0 (  0.0%) |           0 (  0.0%) |           0 (  0.0%) |
| ······tasmlib_arithmetic_u64_and                                                                                              |          48 (  0.1%) |          16 (  0.0%) |           0 (  0.0%) |           0 (  0.0%) |         311 (  1.2%) |
| ······tasmlib_arithmetic_u64_add                                                                                              |          60 (  0.1%) |          32 (  0.1%) |           0 (  0.0%) |           0 (  0.0%) |         244 (  0.9%) |
| ······tasmlib_arithmetic_u64_popcount                                                                                         |          48 (  0.1%) |           8 (  0.0%) |           0 (  0.0%) |           0 (  0.0%) |          90 (  0.3%) |
| ····tasmlib_mmr_verify_from_secret_in_leaf_index_on_stack_auth_path_loop                                                      |        6776 (  8.8%) |        3392 (  6.4%) |           0 (  0.0%) |        1440 (  4.1%) |        5935 ( 22.7%) |
| ······tasmlib_arithmetic_u64_eq                                                                                               |        1708 (  2.2%) |         732 (  1.4%) |           0 (  0.0%) |           0 (  0.0%) |           0 (  0.0%) |
| ······tasmlib_hashing_merkle_step_u64_index                                                                                   |        3600 (  4.7%) |        1440 (  2.7%) |           0 (  0.0%) |        1440 (  4.1%) |        5935 ( 22.7%) |
| ····tasmlib_list_get_element___digest                                                                                         |          56 (  0.1%) |          60 (  0.1%) |          20 (  0.0%) |           0 (  0.0%) |           4 (  0.0%) |
| ··tasmlib_neptune_mutator_get_swbf_indices_1048576_45                                                                         |       17572 ( 22.9%) |       12746 ( 24.1%) |        1880 (  3.4%) |         172 (  0.5%) |       18208 ( 69.5%) |
| ····tasmlib_arithmetic_u128_shift_right_static_3                                                                              |         100 (  0.1%) |          48 (  0.1%) |           0 (  0.0%) |           0 (  0.0%) |         258 (  1.0%) |
| ····tasmlib_arithmetic_u128_shift_left_static_12                                                                              |          92 (  0.1%) |          48 (  0.1%) |           0 (  0.0%) |           0 (  0.0%) |         252 (  1.0%) |
| ····tasmlib_hashing_algebraic_hasher_sample_indices                                                                           |       11088 ( 14.4%) |        7974 ( 15.1%) |         956 (  1.7%) |         120 (  0.3%) |       11796 ( 45.0%) |
| ······tasmlib_list_new___u32                                                                                                  |         116 (  0.2%) |          94 (  0.2%) |          12 (  0.0%) |           0 (  0.0%) |         128 (  0.5%) |
| ········tasmlib_memory_dyn_malloc                                                                                             |         172 (  0.2%) |         154 (  0.3%) |          16 (  0.0%) |           0 (  0.0%) |         256 (  1.0%) |
| ··········tasmlib_memory_dyn_malloc_initialize                                                                                |           4 (  0.0%) |           2 (  0.0%) |           0 (  0.0%) |           0 (  0.0%) |           0 (  0.0%) |
| ······tasmlib_hashing_algebraic_hasher_sample_indices_main_loop                                                               |       10940 ( 14.2%) |        7864 ( 14.9%) |         944 (  1.7%) |         120 (  0.3%) |       11668 ( 44.5%) |
| ········tasmlib_list_length___u32                                                                                             |         896 (  1.2%) |         448 (  0.8%) |         224 (  0.4%) |           0 (  0.0%) |           0 (  0.0%) |
| ········tasmlib_hashing_algebraic_hasher_sample_indices_then_reduce_and_save                                                  |        6300 (  8.2%) |        3780 (  7.1%) |         720 (  1.3%) |           0 (  0.0%) |       11668 ( 44.5%) |
| ··········tasmlib_list_push___u32                                                                                             |        3420 (  4.5%) |        2520 (  4.8%) |         720 (  1.3%) |           0 (  0.0%) |           0 (  0.0%) |
| ········tasmlib_hashing_algebraic_hasher_sample_indices_else_drop_tip                                                         |         120 (  0.2%) |          20 (  0.0%) |           0 (  0.0%) |           0 (  0.0%) |           0 (  0.0%) |
| ····tasmlib_list_higher_order_u32_map_u32_to_u128_add_another_u128                                                            |        6192 (  8.1%) |        4488 (  8.5%) |         924 (  1.7%) |           0 (  0.0%) |        5902 ( 22.5%) |
| ······tasmlib_list_new___u128                                                                                                 |         112 (  0.1%) |          92 (  0.2%) |          12 (  0.0%) |           0 (  0.0%) |         128 (  0.5%) |
| ······tasmlib_list_higher_order_u32_map_u32_to_u128_add_another_u128_loop                                                     |        5964 (  7.8%) |        4336 (  8.2%) |         900 (  1.6%) |           0 (  0.0%) |        5774 ( 22.0%) |
| ··tasmlib_hashing_algebraic_hasher_hash_static_size_180                                                                       |         456 (  0.6%) |         376 (  0.7%) |        1440 (  2.6%) |         968 (  2.7%) |           0 (  0.0%) |
| ····tasmlib_hashing_absorb_multiple_static_size_180                                                                           |         336 (  0.4%) |         256 (  0.5%) |        1440 (  2.6%) |         912 (  2.6%) |           0 (  0.0%) |
| Total                                                                                                                         |       76804 (100.0%) |       52956 (100.0%) |       55865 (100.0%) |       35516 (100.0%) |       26200 (100.0%) |
