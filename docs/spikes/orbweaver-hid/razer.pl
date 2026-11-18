#!/usr/bin/perl
# Razer 90-byte feature-report probe over hidraw (perl-base only).
# usage: razer.pl <hidraw> <tid-hex> <class-hex> <id-hex> <size-hex> [arg-hex ...]
use strict; use warnings;
my ($dev, $tid, $cls, $cid, $size, @args) = @ARGV;
die "usage: razer.pl <hidraw> <tid> <class> <id> <size> [args]\n" unless defined $size;
sub IOC { my ($dir,$nr,$len)=@_; return ($dir<<30)|($len<<16)|(ord('H')<<8)|$nr }
my $SFEAT = IOC(3, 0x06, 91); my $GFEAT = IOC(3, 0x07, 91);
my @r = (0) x 90;
$r[1] = hex $tid; $r[5] = hex $size; $r[6] = hex $cls; $r[7] = hex $cid;
$r[8+$_] = hex $args[$_] for 0..$#args;
my $crc = 0; $crc ^= $r[$_] for 2..87; $r[88] = $crc;
open(my $fh, '+<', $dev) or die "open $dev: $!\n";
my $buf = pack('C*', 0, @r);
ioctl($fh, $SFEAT, $buf) // die "SFEATURE: $!\n";
select(undef, undef, undef, 0.06);
my $in = pack('C*', (0) x 91);
ioctl($fh, $GFEAT, $in) // die "GFEATURE: $!\n";
my @o = unpack('C*', $in); shift @o;
my %st = (0=>'new',1=>'busy',2=>'ok',3=>'fail',4=>'timeout',5=>'unsupported');
my $n = $o[5]; $n = 80 if $n > 80;
printf "status=%s(%d) tid=%02x size=%02x cmd=%02x/%02x args=%s\n", $st{$o[0]}//'?', $o[0], $o[1], $o[5], $o[6], $o[7],
  join(' ', map { sprintf '%02x', $_ } @o[8 .. 7+($n||1)]);
