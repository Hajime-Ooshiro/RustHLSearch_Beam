/// 素数生成（エラトステネスの篩）
pub fn generate_primes(limit: usize) -> Vec<usize> {
    if limit < 2 {
        return Vec::new();
    }
    let mut is_prime = vec![true; limit + 1];
    is_prime[0] = false;
    is_prime[1] = false;

    let sqrt_limit = (limit as f64).sqrt() as usize;
    for p in 2..=sqrt_limit {
        if is_prime[p] {
            let mut step = p * p;
            while step <= limit {
                is_prime[step] = false;
                step += p;
            }
        }
    }

    is_prime
        .iter()
        .enumerate()
        .filter_map(|(n, &p)| if p { Some(n) } else { None })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::generate_primes;

    #[test]
    fn handles_small_limits() {
        assert_eq!(generate_primes(0), Vec::<usize>::new());
        assert_eq!(generate_primes(1), Vec::<usize>::new());
        assert_eq!(generate_primes(2), vec![2]);
        assert_eq!(generate_primes(10), vec![2, 3, 5, 7]);
    }
}
