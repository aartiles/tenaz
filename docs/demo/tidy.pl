#!/usr/bin/env perl
# Tidies Tenaz's output for the demo GIF only: drops the log timestamps
# and levels and the temp directory prefix, leaves out the status lines
# that mean nothing without a real engine, and wraps long lines at words.
use strict;
use warnings;

$| = 1;
my $dir   = $ENV{DEMO_DIR} // '';
my $width = $ENV{DEMO_COLUMNS} // 134;

while (my $line = <STDIN>) {
    chomp $line;
    $line =~ s/^(\e\[\d+m)?\d{4}-\d\d-\d\dT\S+Z(\e\[0m)?\s+(\e\[\d+m)?\s*INFO(\e\[0m)? //;
    $line =~ s/\Q$dir\E\///g if $dir ne '';
    next if $line =~ /^(Model|Preflight|Current|Context budget|Engine|Memory): /;
    my ($indent) = $line =~ /^(\s*)/;
    while (length $line > $width) {
        my $cut = rindex $line, ' ', $width;
        last if $cut <= length $indent;
        print substr($line, 0, $cut), "\n";
        $line = "$indent    " . substr($line, $cut + 1);
    }
    print "$line\n";
}
