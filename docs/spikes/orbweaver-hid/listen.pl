#!/usr/bin/perl
# Print every hidraw input report from the given nodes with a timestamp, for <secs> seconds.
use strict; use warnings; use Time::HiRes qw(time);
my ($secs, @devs) = @ARGV; my %fh; my $rin = '';
for my $d (@devs) { open(my $f, '<', $d) or die "open $d: $!\n"; $fh{fileno $f} = [$d, $f]; vec($rin, fileno $f, 1) = 1; }
my $end = time + $secs; $| = 1;
while ((my $left = $end - time) > 0) {
  my $n = select(my $rout = $rin, undef, undef, $left); last if $n <= 0;
  for my $fd (keys %fh) { next unless vec($rout, $fd, 1);
    my ($d, $f) = @{$fh{$fd}}; my $got = sysread($f, my $b, 128);
    printf "%.3f %s %s\n", time, $d, join(' ', map { sprintf '%02x', $_ } unpack('C*', $b)) if $got; }
}
print "done\n";
