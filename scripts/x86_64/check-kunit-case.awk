# Classify an upstream flat program, not the outer QEMU. isa-debug-exit maps
# successful guest exit(0) to QEMU status 1, failure to 3, whole-test skip to 77.
function reset_case() {
    bad=summaries=tests=skipped=expected_failures=passed=bare_pass=s3=sieve_static=sieve_mapped=sieve_virtual=measured=0
}
function record(line, fields, parts, item, i, words) {
    if (line ~ /^FAIL:|^KFAIL:|^XPASS:|^ABORT:|^PANIC:|unexpected failures|known failures|Assertion failed/) bad=1
    if (line ~ /^SUMMARY: /) {
        summaries++
        if (line !~ /^SUMMARY: [0-9]+ tests(, [0-9]+ expected failures)?(, [0-9]+ skipped)?$/) bad=1
        split(line, words, " "); tests=words[2]
        fields=split(line, parts, ", ")
        for (i=2; i<=fields; i++) {
            split(parts[i], item, " ")
            if (item[2] == "skipped") skipped=item[1]
            if (item[2] == "expected") expected_failures=item[1]
        }
        if (tests > 1000000000 || skipped + expected_failures > tests) bad=1
    }
    if (line ~ /^PASS: /) passed++
    if (line == "PASS") bare_pass++
    if (line ~ /^PM1a event registers at /) s3++
    if (line == "static:78498 out of 1000000") sieve_static++
    if (line == "mapped:78498 out of 1000000") sieve_mapped++
    if (line == "virtual:5761455 out of 100000000") sieve_virtual++
    if (split(line, words, " ") == 2 && argument != "-" && words[1] == argument && words[2] ~ /^[0-9]+$/) measured++
}
function classify(status, name) {
    if (bad || summaries > 1) return 1
    if (status == 77 && summaries == 1 && tests == skipped) return 4
    if (status != 1) return 1
    # Legacy programs have no report_summary(). Require their own completion
    # records as well as the debug-exit status; an empty successful VM fails.
    if (name == "realmode") return summaries || passed == 0
    if (name == "rmap_chain") return summaries || bare_pass != 1
    if (name == "s3") return summaries || s3 != 1
    if (name == "sieve") return summaries || sieve_static != 1 || sieve_mapped != 1 || sieve_virtual != 3
    if (name ~ /^vmexit_/) return summaries || measured != 1
    return summaries != 1 || tests <= skipped
}
BEGIN {
    if (matrix) {
        while ((getline line < manifest) > 0) {
            if (line ~ /^#/ || line == "") continue
            split(line, column, "|")
            if (selection != "all" && selection != column[1]) continue
            expected[++count]=column[1]
            starts[column[1]]="thin-hv: KVM unit start name=" column[1] " cpus=" column[3] " memory_mib=" column[4] " timeout_seconds=" column[5] " accel=kvm"
            arguments[column[1]]=column[7]
        }
        close(manifest)
        if (!count) matrix_bad=1
    }
}
{
    sub(/\r$/, "")
    if (!matrix) { record($0); next }
    if ($0 ~ /Kernel panic|Oops:|BUG:|thin-hv: KVM unit matrix FAIL/) matrix_bad=1
    if ($0 == "thin-hv: KVM unit matrix begin backend=" backend " selection=" selection " l1_cpus=1") {
        if (begin || current || complete) matrix_bad=1
        begin++
    } else if ($0 ~ /^thin-hv: KVM unit start /) {
        if (begin != 1 || active || complete || current >= count) matrix_bad=1
        name=expected[++current]; argument=arguments[name]
        if ($0 != starts[name]) matrix_bad=1
        active=1; reset_case()
    } else if ($0 ~ /^KUNIT: /) {
        if (!active) matrix_bad=1
        record(substr($0, 8))
    } else if ($0 ~ /^thin-hv: KVM unit exit /) {
        if (!active || NF != 7) matrix_bad=1
        status=$6; sub(/^process_exit=/, "", status)
        if (status !~ /^[0-9]+$/ || status > 255) matrix_bad=1
        result=classify(status, name)
        outcome=result == 0 ? "PASS" : (result == 4 ? "SKIP" : "FAIL")
        if ($0 != "thin-hv: KVM unit exit name=" name " process_exit=" status " outcome=" outcome) matrix_bad=1
        if (result == 0) ok++; else if (result == 4) unavailable++; else failed++
        active=0; exits++
    } else if ($0 ~ /^thin-hv: KVM unit matrix complete /) {
        if (begin != 1 || active || exits != count || complete ||
            $0 != "thin-hv: KVM unit matrix complete backend=" backend " selection=" selection " cases=" count " passed=" ok+0 " failed=" failed+0 " skipped=" unavailable+0) matrix_bad=1
        complete++
    } else if ($0 == "thin-hv: KVM unit matrix poweroff requested") {
        if (complete != 1 || poweroff) matrix_bad=1
        poweroff++
    } else if ($0 ~ /^thin-hv: KVM unit /) matrix_bad=1
}
END {
    if (!matrix) exit classify(status, name)
    exit (matrix_bad || begin != 1 || complete != 1 || poweroff != 1 || active || failed || unavailable)
}
