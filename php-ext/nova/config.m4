PHP_ARG_ENABLE([nova],
  [whether to enable NOVA worker mode support],
  [AS_HELP_STRING([--enable-nova], [Enable NOVA worker mode support])],
  [no])

if test "$PHP_NOVA" != "no"; then
  PHP_NEW_EXTENSION(nova, nova.c, $ext_shared)
fi
