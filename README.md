# Sidebar overflow layout evidence

Browser layout fixture using the ConnectionTree.vue and TreeItem.vue styles from upstream main 239183faf. The DOM reproduces search row layout, ordinary and virtual wrappers, horizontal scrolling on/off, and depths 0/2/5. These are isolated layout screenshots, not a live database session.

- sidebar-before.png: viewport-width content plus outer indentation makes trailing controls overflow.
- sidebar-after.png: indentation and end padding are included inside the viewport-width content.

Browser bounds checks passed for 36 combinations (widths 240/340/480 px).
